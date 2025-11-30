use crate::models::*;
use chrono::{DateTime, Utc};
use futures::future::join_all;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::{Client, StatusCode};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

static ZKILL_URL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"zkillboard\.com/(?P<type>\w+)/(?P<id>\d+)").unwrap());

pub async fn fetch_zkill_data(
    user_url: &str,
    state: &Arc<AppState>,
    start_cutoff: DateTime<Utc>,
) -> Result<Vec<Killmail>, String> {
    // 1. Regex Parse
    let caps = ZKILL_URL_REGEX
        .captures(user_url)
        .ok_or("Invalid ZKillboard Link format")?;
    let entity_type = caps.name("type").map(|m| m.as_str()).unwrap_or("");
    let entity_id = caps.name("id").map(|m| m.as_str()).unwrap_or("");

    let api_type = match entity_type {
        "corporation" => "corporationID",
        "alliance" => "allianceID",
        "character" => "characterID",
        "system" => "solarSystemID",
        "region" => "regionID",
        _ => return Err(format!("Unsupported entity type: {}", entity_type)),
    };

    let client = Client::builder()
        .user_agent("EveLooter/1.9 (maintainer: admin@example.com)")
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()
        .map_err(|e| e.to_string())?;

    let mut all_raw_items: Vec<RawZKillItem> = Vec::new();
    let max_pages = 10;

    // 2. PAGINATION LOOP
    for page in 1..=max_pages {
        let page_url = if page == 1 {
            format!("https://zkillboard.com/api/{}/{}/", api_type, entity_id)
        } else {
            format!(
                "https://zkillboard.com/api/{}/{}/page/{}/",
                api_type, entity_id, page
            )
        };

        info!("Fetching Page {} from ZKill: {}", page, page_url);

        let resp = client
            .get(&page_url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!(
                "ZKillboard Error on page {}: {}",
                page,
                resp.status()
            ));
        }

        let page_items: Vec<RawZKillItem> = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse ZKill JSON on page {}: {}", page, e))?;

        if page_items.is_empty() {
            info!("Page {} was empty, stopping fetch.", page);
            break;
        }

        // --- HYDRATE IMMEDIATELY TO CHECK DATES ---

        let mut to_fetch = Vec::new();
        {
            let cache = state.esi_cache.lock().unwrap();
            for item in &page_items {
                if !cache.contains_key(&item.killmail_id) {
                    to_fetch.push(item);
                }
            }
        }

        if !to_fetch.is_empty() {
            info!(
                "Page {}: Fetching details for {} new kills from ESI...",
                page,
                to_fetch.len()
            );
            let mut tasks = Vec::new();

            for item in to_fetch.iter() {
                let client_clone = client.clone();
                let id = item.killmail_id;
                let hash = item.zkb.hash.clone();

                tasks.push(async move {
                    let esi_url = format!(
                        "https://esi.evetech.net/v1/killmails/{}/{}/?datasource=tranquility",
                        id, hash
                    );
                    match client_clone.get(&esi_url).send().await {
                        Ok(r) => {
                            let status = r.status();
                            if status.is_success() {
                                match r.json::<EsiKillmail>().await {
                                    Ok(d) => Ok(Some((id, d))),
                                    Err(e) => {
                                        error!("Failed to parse ESI JSON for {}: {}", id, e);
                                        Ok(None)
                                    }
                                }
                            } else {
                                // CRITICAL: Return the error status so we can check for rate limits
                                Err(status)
                            }
                        }
                        Err(e) => {
                            error!("Network error for {}: {}", id, e);
                            Ok(None)
                        }
                    }
                });
            }

            let results = join_all(tasks).await;

            // Check for RATE LIMITS (420 or 429) or Server Errors
            for res in &results {
                if let Err(status) = res {
                    if status.as_u16() == 420 || *status == StatusCode::TOO_MANY_REQUESTS {
                        error!(
                            "ESI Rate Limit Triggered (Status {}). Aborting fetch.",
                            status
                        );
                        return Err(format!(
                            "ESI Rate Limit Triggered (Status {}). Try again later.",
                            status
                        ));
                    }
                    if status.is_server_error() {
                        warn!("ESI Server Error encountered: {}", status);
                    }
                }
            }

            {
                let mut cache = state.esi_cache.lock().unwrap();
                for res in results {
                    if let Ok(Some((id, data))) = res {
                        cache.insert(id, data);
                    }
                }
            }
        }

        let (oldest_in_batch, batch_valid) = {
            let cache = state.esi_cache.lock().unwrap();
            let mut oldest = Utc::now();
            let mut valid = false;

            for item in &page_items {
                if let Some(esi_data) = cache.get(&item.killmail_id) {
                    if let Ok(t) = DateTime::parse_from_rfc3339(&esi_data.killmail_time) {
                        let t_utc = t.with_timezone(&Utc);
                        if t_utc < oldest {
                            oldest = t_utc;
                        }
                    }
                    valid = true;
                }
            }
            (oldest, valid)
        };

        all_raw_items.extend(page_items);

        if batch_valid && oldest_in_batch < start_cutoff {
            info!(
                "Reached kills older than start date ({} < {}). Stopping fetch.",
                oldest_in_batch, start_cutoff
            );
            break;
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    info!("Total kills fetched from ZKill: {}", all_raw_items.len());

    // 3. Pre-filter zero value kills
    let worthwhile_kills: Vec<RawZKillItem> = all_raw_items
        .into_iter()
        .filter(|k| k.zkb.dropped_value > 0.0)
        .collect();

    // 4. Resolve Names
    let mut ids_to_resolve = HashSet::new();
    {
        let esi_cache = state.esi_cache.lock().unwrap();
        let name_cache = state.name_cache.lock().unwrap();

        for item in &worthwhile_kills {
            if let Some(esi_data) = esi_cache.get(&item.killmail_id) {
                if let Some(id) = esi_data.victim.character_id {
                    if !name_cache.contains_key(&id) {
                        ids_to_resolve.insert(id);
                    }
                }
                if let Some(id) = esi_data.victim.corporation_id {
                    if !name_cache.contains_key(&id) {
                        ids_to_resolve.insert(id);
                    }
                }
                if !name_cache.contains_key(&esi_data.victim.ship_type_id) {
                    ids_to_resolve.insert(esi_data.victim.ship_type_id);
                }
                if !name_cache.contains_key(&esi_data.solar_system_id) {
                    ids_to_resolve.insert(esi_data.solar_system_id);
                }
                for att in &esi_data.attackers {
                    if let Some(id) = att.character_id {
                        if !name_cache.contains_key(&id) {
                            ids_to_resolve.insert(id);
                        }
                    }
                }
            }
        }
    }

    if !ids_to_resolve.is_empty() {
        info!(
            "Resolving names for {} new entities via ESI",
            ids_to_resolve.len()
        );
        let ids_vec: Vec<i32> = ids_to_resolve.into_iter().collect();

        for chunk in ids_vec.chunks(1000) {
            let url = "https://esi.evetech.net/v1/universe/names/?datasource=tranquility";
            let resp = client.post(url).json(&chunk).send().await;
            match resp {
                Ok(r) => {
                    if r.status().is_success() {
                        if let Ok(entries) = r.json::<Vec<EsiNameEntry>>().await {
                            let mut name_cache = state.name_cache.lock().unwrap();
                            for entry in entries {
                                name_cache.insert(entry.id, entry.name);
                            }
                        }
                    } else {
                        // Handle Rate Limit on Name Resolution
                        if r.status().as_u16() == 420 || r.status() == StatusCode::TOO_MANY_REQUESTS
                        {
                            error!(
                                "ESI Rate Limit Triggered during Name Resolution. Status: {}",
                                r.status()
                            );
                            return Err(
                                "ESI Rate Limit Exceeded during name resolution.".to_string()
                            );
                        }
                        warn!("ESI Name Resolution failed: {}", r.status());
                    }
                }
                Err(e) => error!("Failed to contact ESI Name Resolution endpoint: {}", e),
            }
        }
    }

    // 5. Construct Final Objects
    let mut final_kills = Vec::new();
    let esi_cache = state.esi_cache.lock().unwrap();
    let name_cache = state.name_cache.lock().unwrap();

    for item in worthwhile_kills {
        if let Some(esi_data) = esi_cache.get(&item.killmail_id) {
            let disp_victim = Victim {
                character_id: esi_data.victim.character_id,
                character_name: esi_data
                    .victim
                    .character_id
                    .and_then(|id| name_cache.get(&id).cloned()),
                corporation_name: esi_data
                    .victim
                    .corporation_id
                    .and_then(|id| name_cache.get(&id).cloned()),
                ship_type_id: esi_data.victim.ship_type_id,
                ship_type_name: name_cache.get(&esi_data.victim.ship_type_id).cloned(),
            };

            let mut disp_attackers = Vec::new();
            for att in &esi_data.attackers {
                disp_attackers.push(Attacker {
                    character_id: att.character_id,
                    character_name: att.character_id.and_then(|id| name_cache.get(&id).cloned()),
                    corporation_id: att.corporation_id,
                    final_blow: att.final_blow,
                });
            }

            final_kills.push(Killmail {
                killmail_id: item.killmail_id,
                zkb: item.zkb.clone(),
                victim: Some(disp_victim),
                attackers: disp_attackers,
                killmail_time: esi_data.killmail_time.clone(),
                formatted_dropped: format_isk(item.zkb.dropped_value),
                solar_system_id: esi_data.solar_system_id,
                solar_system_name: name_cache.get(&esi_data.solar_system_id).cloned(),
                is_active: true,
            });
        }
    }

    Ok(final_kills)
}
