use crate::models::*;
use futures::future::join_all;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

static ZKILL_URL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"zkillboard\.com/(?P<type>\w+)/(?P<id>\d+)").unwrap());

pub async fn fetch_zkill_data(
    user_url: &str,
    state: &Arc<AppState>,
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

    let zkill_list_url = format!("https://zkillboard.com/api/{}/{}/", api_type, entity_id);
    info!("Starting fetch for: {} (ID: {})", entity_type, entity_id);
    debug!("ZKill API URL: {}", zkill_list_url);

    // 2. Setup Client
    let client = Client::builder()
        .user_agent("EveLooter/1.7 (maintainer: admin@example.com)")
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()
        .map_err(|e| {
            error!("Failed to build HTTP client: {}", e);
            e.to_string()
        })?;

    // 3. Fetch ZKill List
    let resp = client.get(&zkill_list_url).send().await.map_err(|e| {
        error!("Failed to connect to ZKillboard: {}", e);
        e.to_string()
    })?;

    if !resp.status().is_success() {
        warn!("ZKillboard returned error status: {}", resp.status());
        return Err(format!("ZKillboard List Error: {}", resp.status()));
    }

    let raw_list: Vec<RawZKillItem> = resp.json().await.map_err(|e| {
        error!("Failed to parse ZKill JSON: {}", e);
        format!("Failed to parse ZKill List: {}", e)
    })?;

    info!("ZKill returned {} items", raw_list.len());

    // 4. Pre-filter zero value kills
    let worthwhile_kills: Vec<RawZKillItem> = raw_list
        .into_iter()
        .filter(|k| k.zkb.dropped_value > 0.0)
        .collect();

    debug!("Kills with loot > 0: {}", worthwhile_kills.len());

    // 5. Check Cache for ESI Data
    let mut to_fetch = Vec::new();
    {
        let cache = state.esi_cache.lock().unwrap();
        for item in &worthwhile_kills {
            if !cache.contains_key(&item.killmail_id) {
                to_fetch.push(item);
            }
        }
    }

    // 6. Fetch Missing Data from ESI
    if !to_fetch.is_empty() {
        info!(
            "ESI Cache Miss: Need to fetch details for {} kills",
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
                // debug!("Fetching ESI detail: {}", id); // Uncomment for very verbose logs
                match client_clone.get(&esi_url).send().await {
                    Ok(r) => {
                        if r.status().is_success() {
                            r.json::<EsiKillmail>().await.ok().map(|d| (id, d))
                        } else {
                            warn!("ESI returned {} for kill {}", r.status(), id);
                            None
                        }
                    }
                    Err(e) => {
                        error!("ESI Network error for kill {}: {}", id, e);
                        None
                    }
                }
            });
        }

        let new_results: Vec<Option<(i32, EsiKillmail)>> = join_all(tasks).await;
        let success_count = new_results.iter().filter(|r| r.is_some()).count();
        info!(
            "Successfully hydrated {}/{} kills from ESI",
            success_count,
            to_fetch.len()
        );

        let mut cache = state.esi_cache.lock().unwrap();
        for res in new_results {
            if let Some((id, data)) = res {
                cache.insert(id, data);
            }
        }
    } else {
        debug!(
            "All {} kills found in local ESI cache.",
            worthwhile_kills.len()
        );
    }

    // 7. Resolve Names
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
            if let Ok(r) = resp {
                if r.status().is_success() {
                    if let Ok(entries) = r.json::<Vec<EsiNameEntry>>().await {
                        let mut name_cache = state.name_cache.lock().unwrap();
                        for entry in &entries {
                            name_cache.insert(entry.id, entry.name.clone());
                        }
                        debug!("Resolved batch of {} names", entries.len());
                    } else {
                        error!("Failed to parse ESI Name response");
                    }
                } else {
                    warn!("ESI Name Resolution failed: {}", r.status());
                }
            } else {
                error!("Failed to contact ESI Name Resolution endpoint");
            }
        }
    } else {
        debug!("All names resolved in local cache.");
    }

    // 8. Construct Final Objects
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
            };

            let mut disp_attackers = Vec::new();
            for att in &esi_data.attackers {
                disp_attackers.push(Attacker {
                    character_id: att.character_id,
                    character_name: att.character_id.and_then(|id| name_cache.get(&id).cloned()),
                    corporation_id: att.corporation_id,
                });
            }

            final_kills.push(Killmail {
                killmail_id: item.killmail_id,
                zkb: item.zkb.clone(),
                victim: Some(disp_victim),
                attackers: disp_attackers,
                killmail_time: esi_data.killmail_time.clone(),
                formatted_dropped: format_isk(item.zkb.dropped_value),
                is_active: true,
            });
        }
    }

    info!("Logic complete. Returning {} kills.", final_kills.len());

    Ok(final_kills)
}
