use askama::Template;
use axum::{
    extract::{Form, State},
    response::Html,
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Duration, Utc};
use futures::future::join_all;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

// --- Regex ---
static ZKILL_URL_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"zkillboard\.com/(?P<type>\w+)/(?P<id>\d+)").unwrap());

// --- Helper: Human Readable ISK ---
fn format_isk(amount: f64) -> String {
    let abs_amount = amount.abs();
    if abs_amount >= 1_000_000_000_000.0 {
        format!("{:.2}t", amount / 1_000_000_000_000.0)
    } else if abs_amount >= 1_000_000_000.0 {
        format!("{:.2}b", amount / 1_000_000_000.0)
    } else if abs_amount >= 1_000_000.0 {
        format!("{:.2}m", amount / 1_000_000.0)
    } else if abs_amount >= 1_000.0 {
        format!("{:.2}k", amount / 1_000.0)
    } else {
        format!("{:.0}", amount)
    }
}

// --- Data Structures ---

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Killmail {
    killmail_id: i32,
    zkb: ZkbStats,
    victim: Option<Victim>,
    attackers: Vec<Attacker>,
    killmail_time: String,
    // Pre-formatted string for display (e.g. "1.5b")
    formatted_dropped: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawZKillItem {
    killmail_id: i32,
    zkb: ZkbStats,
}

#[derive(Debug, Clone, Deserialize)]
struct EsiKillmail {
    killmail_time: String,
    victim: EsiVictim,
    attackers: Vec<EsiAttacker>,
}

#[derive(Debug, Clone, Deserialize)]
struct EsiVictim {
    character_id: Option<i32>,
    corporation_id: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
struct EsiAttacker {
    character_id: Option<i32>,
    corporation_id: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
struct EsiNameEntry {
    id: i32,
    name: String,
    category: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ZkbStats {
    locationID: i32,
    hash: String,
    fittedValue: f64,
    droppedValue: f64,
    destroyedValue: f64,
    totalValue: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Victim {
    character_id: Option<i32>,
    character_name: Option<String>,
    corporation_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Attacker {
    character_id: Option<i32>,
    character_name: Option<String>,
    corporation_id: Option<i32>,
}

struct AppState {
    current_kills: Mutex<Vec<Killmail>>,
    character_map: Mutex<HashMap<String, String>>,
    esi_cache: Mutex<HashMap<i32, EsiKillmail>>,
    name_cache: Mutex<HashMap<i32, String>>,
}

// --- HTML Templates ---

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    kills: Vec<Killmail>,
    mapping_text: String,
    zkill_link: String,
    days_back: u64,
    // Display Strings
    total_payout_str: String,
    payout_per_main_str: String,
    // Tuple: (Name, Formatted Amount String)
    share_breakdown: Vec<(String, String)>,
    error_msg: Option<String>,
}

#[derive(Deserialize, Debug)]
struct FetchParams {
    zkill_link: String,
    mapping_input: String,
    excluded_kills: Option<String>,
    #[serde(default = "default_days")]
    days_back: u64,
}

fn default_days() -> u64 {
    7
}

// --- Main ---

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = Arc::new(AppState {
        current_kills: Mutex::new(Vec::new()),
        character_map: Mutex::new(HashMap::new()),
        esi_cache: Mutex::new(HashMap::new()),
        name_cache: Mutex::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/", get(show_index))
        .route("/process", post(process_data))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("EVE Looter running on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- Handlers ---

async fn show_index() -> Html<String> {
    let template = IndexTemplate {
        kills: vec![],
        mapping_text: "".to_string(),
        zkill_link: "".to_string(),
        days_back: 7,
        total_payout_str: "0".to_string(),
        payout_per_main_str: "0".to_string(),
        share_breakdown: vec![],
        error_msg: None,
    };
    Html(template.render().unwrap())
}

async fn process_data(
    State(state): State<Arc<AppState>>,
    Form(params): Form<FetchParams>,
) -> Html<String> {
    // 1. Update Mapping
    {
        let mut map_guard = state.character_map.lock().unwrap();
        map_guard.clear();
        for line in params.mapping_input.lines() {
            if let Some((alt, main)) = line.split_once([':', '=']) {
                map_guard.insert(alt.trim().to_string(), main.trim().to_string());
            }
        }
    }

    // 2. Fetch Data
    let fetch_result = if !params.zkill_link.is_empty() {
        Some(fetch_zkill_data(&params.zkill_link, &state).await)
    } else {
        None
    };

    // 3. Update State
    let mut kills_guard = state.current_kills.lock().unwrap();
    let mut error_msg = None;

    if let Some(res) = fetch_result {
        match res {
            Ok(fetched_kills) => {
                *kills_guard = fetched_kills;
            }
            Err(e) => {
                println!("Error fetching data: {}", e);
                if kills_guard.is_empty() {
                    error_msg = Some(format!("Failed to fetch: {}", e));
                }
            }
        }
    }

    let current_map = state.character_map.lock().unwrap().clone();

    // 4. Filter by Time
    let cutoff = Utc::now() - Duration::days(params.days_back as i64);

    let time_filtered_kills: Vec<Killmail> = kills_guard
        .iter()
        .filter(|k| {
            if let Ok(t) = DateTime::parse_from_rfc3339(&k.killmail_time) {
                t.with_timezone(&Utc) > cutoff
            } else {
                false
            }
        })
        .cloned()
        .collect();

    // 5. Filter Excluded IDs
    let excluded_ids: HashSet<i32> = params
        .excluded_kills
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let final_kills: Vec<Killmail> = time_filtered_kills
        .into_iter()
        .filter(|k| !excluded_ids.contains(&k.killmail_id))
        .collect();

    // 6. Calculate Payout
    let mut total_dropped_value = 0.0;
    let mut unique_humans: HashSet<String> = HashSet::new();

    for kill in &final_kills {
        total_dropped_value += kill.zkb.droppedValue;
        for attacker in &kill.attackers {
            if let Some(name) = &attacker.character_name {
                let main = current_map.get(name).unwrap_or(name);
                unique_humans.insert(main.clone());
            }
        }
    }

    let human_count = unique_humans.len().max(1) as f64;
    let payout_per_human = total_dropped_value / human_count;

    // Create sorted breakdown with formatted strings
    let mut breakdown: Vec<_> = unique_humans
        .into_iter()
        .map(|h| (h, payout_per_human))
        .collect();
    breakdown.sort_by(|a, b| a.0.cmp(&b.0));

    let formatted_breakdown: Vec<(String, String)> = breakdown
        .into_iter()
        .map(|(name, val)| (name, format_isk(val)))
        .collect();

    let template = IndexTemplate {
        kills: final_kills,
        mapping_text: params.mapping_input,
        zkill_link: params.zkill_link,
        days_back: params.days_back,
        total_payout_str: format_isk(total_dropped_value),
        payout_per_main_str: format_isk(payout_per_human),
        share_breakdown: formatted_breakdown,
        error_msg,
    };

    Html(template.render().unwrap())
}

// --- Logic ---

async fn fetch_zkill_data(user_url: &str, state: &Arc<AppState>) -> Result<Vec<Killmail>, String> {
    // A. Parse URL
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
    println!("Step 1: Fetching List from ZKill: {}", zkill_list_url);

    let client = Client::builder()
        .user_agent("EveLooter/1.2 (maintainer: admin@example.com)")
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()
        .map_err(|e| e.to_string())?;

    // B. Fetch ZKill Summary
    let resp = client
        .get(&zkill_list_url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("ZKillboard List Error: {}", resp.status()));
    }

    let raw_list: Vec<RawZKillItem> = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse ZKill List: {}", e))?;

    // C. Cache Check (ESI Structure)
    let mut to_fetch = Vec::new();
    {
        let cache = state.esi_cache.lock().unwrap();
        for item in &raw_list {
            if !cache.contains_key(&item.killmail_id) {
                to_fetch.push(item);
            }
        }
    }

    // D. Fetch Missing ESI Data
    if !to_fetch.is_empty() {
        println!("Fetching {} items from ESI...", to_fetch.len());
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
                        if r.status().is_success() {
                            r.json::<EsiKillmail>().await.ok().map(|d| (id, d))
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                }
            });
        }
        let new_results: Vec<Option<(i32, EsiKillmail)>> = join_all(tasks).await;

        let mut cache = state.esi_cache.lock().unwrap();
        for res in new_results {
            if let Some((id, data)) = res {
                cache.insert(id, data);
            }
        }
    }

    // E. NAME RESOLUTION STEP
    let mut ids_to_resolve = HashSet::new();
    {
        let esi_cache = state.esi_cache.lock().unwrap();
        let name_cache = state.name_cache.lock().unwrap();

        for item in &raw_list {
            if let Some(esi_data) = esi_cache.get(&item.killmail_id) {
                // Victim
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
                // Attackers
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
        println!("Resolving {} names via ESI...", ids_to_resolve.len());
        let ids_vec: Vec<i32> = ids_to_resolve.into_iter().collect();

        for chunk in ids_vec.chunks(1000) {
            let url = "https://esi.evetech.net/v1/universe/names/?datasource=tranquility";
            let resp = client.post(url).json(&chunk).send().await;

            if let Ok(r) = resp {
                if r.status().is_success() {
                    if let Ok(entries) = r.json::<Vec<EsiNameEntry>>().await {
                        let mut name_cache = state.name_cache.lock().unwrap();
                        for entry in entries {
                            name_cache.insert(entry.id, entry.name);
                        }
                    }
                }
            }
        }
    }

    // F. Construct Final Structs
    let mut final_kills = Vec::new();
    let esi_cache = state.esi_cache.lock().unwrap();
    let name_cache = state.name_cache.lock().unwrap();

    for item in raw_list {
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
                zkb: item.zkb.clone(), // Clone to get droppedValue
                victim: Some(disp_victim),
                attackers: disp_attackers,
                killmail_time: esi_data.killmail_time.clone(),
                // FORMATTED VALUE CALCULATED HERE
                formatted_dropped: format_isk(item.zkb.droppedValue),
            });
        }
    }

    Ok(final_kills)
}
