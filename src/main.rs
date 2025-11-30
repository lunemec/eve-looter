mod logic;
mod models;

use crate::logic::fetch_zkill_data;
use crate::models::*;

use askama::Template;
use axum::{
    extract::{Form, State},
    response::Html,
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, error, info}; // Import tracing macros

// --- View Models ---

struct BeneficiaryDisplay {
    name: String,
    formatted_amount: String,
    is_active: bool,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    kills: Vec<Killmail>,
    mapping_text: String,
    zkill_link: String,
    start_date: String,
    end_date: String,
    total_payout_str: String,
    total_humans: usize,
    beneficiaries: Vec<BeneficiaryDisplay>,
    error_msg: Option<String>,
}

#[derive(Deserialize, Debug)]
struct FetchParams {
    zkill_link: String,
    mapping_input: String,
    excluded_kills: Option<String>,
    excluded_beneficiaries: Option<String>,
    #[serde(default)]
    start_date: String,
    #[serde(default)]
    end_date: String,
}

// --- Main ---

#[tokio::main]
async fn main() {
    // Initialize tracing (logs to stdout by default)
    tracing_subscriber::fmt::init();

    let state = Arc::new(AppState::new());

    let app = Router::new()
        .route("/", get(show_index))
        .route("/process", post(process_data))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    info!("EVE Looter running on http://{}", addr); // Changed to info!
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- Handlers ---

async fn show_index() -> Html<String> {
    let now = Utc::now();
    let start = now - Duration::days(7);

    let template = IndexTemplate {
        kills: vec![],
        mapping_text: "".to_string(),
        zkill_link: "".to_string(),
        start_date: start.format("%Y-%m-%d").to_string(),
        end_date: now.format("%Y-%m-%d").to_string(),
        total_payout_str: "0".to_string(),
        total_humans: 0,
        beneficiaries: vec![],
        error_msg: None,
    };
    Html(template.render().unwrap())
}

async fn process_data(
    State(state): State<Arc<AppState>>,
    Form(params): Form<FetchParams>,
) -> Html<String> {
    info!("Received process request for link: {}", params.zkill_link);

    // 1. Update Mapping
    {
        let mut map_guard = state.character_map.lock().unwrap();
        map_guard.clear();
        for line in params.mapping_input.lines() {
            if let Some((alt, main)) = line.split_once([':', '=']) {
                map_guard.insert(alt.trim().to_string(), main.trim().to_string());
            }
        }
        debug!("Updated character mapping with {} entries", map_guard.len());
    }

    // 2. Fetch Data
    let fetch_result = if !params.zkill_link.is_empty() {
        Some(fetch_zkill_data(&params.zkill_link, &state).await)
    } else {
        None
    };

    let mut kills_guard = state.current_kills.lock().unwrap();
    let mut error_msg = None;

    if let Some(res) = fetch_result {
        match res {
            Ok(fetched_kills) => {
                *kills_guard = fetched_kills;
            }
            Err(e) => {
                error!("Error fetching data: {}", e);
                if kills_guard.is_empty() {
                    error_msg = Some(format!("Failed to fetch: {}", e));
                }
            }
        }
    }

    // 3. Time Filter
    let start_cutoff = NaiveDate::parse_from_str(&params.start_date, "%Y-%m-%d")
        .unwrap_or_else(|_| (Utc::now() - Duration::days(7)).date_naive())
        .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
        .and_utc();

    let end_cutoff = NaiveDate::parse_from_str(&params.end_date, "%Y-%m-%d")
        .unwrap_or_else(|_| Utc::now().date_naive())
        .and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())
        .and_utc();

    // Enforce 30 Day Limit: Error if window is too large
    if (end_cutoff - start_cutoff).num_days() > 30 {
        let template = IndexTemplate {
            kills: vec![],
            mapping_text: params.mapping_input,
            zkill_link: params.zkill_link,
            start_date: params.start_date,
            end_date: params.end_date,
            total_payout_str: "0".to_string(),
            total_humans: 0,
            beneficiaries: vec![],
            error_msg: Some(
                "Timeframe exceeds 30 days. Please select a shorter range.".to_string(),
            ),
        };
        return Html(template.render().unwrap());
    }

    let excluded_ids: HashSet<i32> = params
        .excluded_kills
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    // 4. Parse Excluded Beneficiaries
    let excluded_names: HashSet<String> = params
        .excluded_beneficiaries
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // 5. Filter & Tag Active Kills
    let final_kills: Vec<Killmail> = kills_guard
        .iter()
        .filter(|k| {
            if k.zkb.dropped_value <= 0.0 {
                return false;
            }
            if let Ok(t) = DateTime::parse_from_rfc3339(&k.killmail_time) {
                let t_utc = t.with_timezone(&Utc);
                t_utc >= start_cutoff && t_utc <= end_cutoff
            } else {
                false
            }
        })
        .map(|k| {
            let mut km = k.clone();
            km.is_active = !excluded_ids.contains(&k.killmail_id);
            km
        })
        .collect();

    debug!(
        "Filtered kills: {} total, {} excluded by user",
        final_kills.len(),
        excluded_ids.len()
    );

    // 6. Calculate Payout
    let current_map = state.character_map.lock().unwrap().clone();

    let mut all_seen_mains: HashSet<String> = HashSet::new();
    let mut main_wallets: HashMap<String, f64> = HashMap::new();
    let mut total_dropped_value = 0.0;

    for kill in &final_kills {
        if !kill.is_active {
            continue;
        }

        total_dropped_value += kill.zkb.dropped_value;

        // A. Identify all potential participants
        let mut kill_participants: HashSet<String> = HashSet::new();

        for attacker in &kill.attackers {
            if let Some(name) = &attacker.character_name {
                let main = current_map.get(name).unwrap_or(name);
                all_seen_mains.insert(main.clone());

                // Only include in division if NOT excluded
                if !excluded_names.contains(main) {
                    kill_participants.insert(main.clone());
                }
            }
        }

        // B. Calculate Share
        if kill_participants.is_empty() {
            continue;
        }

        let participant_count = kill_participants.len() as f64;
        let share_per_pilot = kill.zkb.dropped_value / participant_count;

        for main in kill_participants {
            *main_wallets.entry(main).or_insert(0.0) += share_per_pilot;
        }
    }

    // 7. Construct Display List
    let mut beneficiaries = Vec::new();
    for main in all_seen_mains {
        let amount = *main_wallets.get(&main).unwrap_or(&0.0);
        beneficiaries.push(BeneficiaryDisplay {
            name: main.clone(),
            formatted_amount: format_isk(amount),
            is_active: !excluded_names.contains(&main),
        });
    }
    beneficiaries.sort_by(|a, b| a.name.cmp(&b.name));

    let active_humans = beneficiaries.iter().filter(|b| b.is_active).count();

    info!(
        "Calculation complete. Total Value: {}, Active Pilots: {}",
        format_isk(total_dropped_value),
        active_humans
    );

    let template = IndexTemplate {
        kills: final_kills,
        mapping_text: params.mapping_input,
        zkill_link: params.zkill_link,
        // Send back the ACTUAL dates used (in case we clamped them)
        start_date: start_cutoff.format("%Y-%m-%d").to_string(),
        end_date: end_cutoff.format("%Y-%m-%d").to_string(),
        total_payout_str: format_isk(total_dropped_value),
        total_humans: active_humans,
        beneficiaries,
        error_msg,
    };

    Html(template.render().unwrap())
}
