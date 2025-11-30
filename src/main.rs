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
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;
use tracing::{debug, error, info};

// --- View Models ---

struct BeneficiaryDisplay {
    name: String,
    formatted_amount: String,
    is_active: bool,
}

struct DailyGroup {
    date_display: String,
    kills: Vec<Killmail>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    daily_groups: Vec<DailyGroup>,
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
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "eve_looter=info,tower_http=debug");
    }

    tracing_subscriber::fmt::init();
    let state = Arc::new(AppState::new());

    let app = Router::new()
        .route("/", get(show_index))
        .route("/process", post(process_data))
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    info!("EVE Looter running on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- Handlers ---

async fn show_index() -> Html<String> {
    let now = Utc::now();
    let start = now - Duration::days(7);

    let template = IndexTemplate {
        daily_groups: vec![],
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
    info!("Processing request for: {}", params.zkill_link);

    // 1. Time Filter Setup
    let start_cutoff = NaiveDate::parse_from_str(&params.start_date, "%Y-%m-%d")
        .unwrap_or_else(|_| (Utc::now() - Duration::days(7)).date_naive())
        .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
        .and_utc();

    let end_cutoff = NaiveDate::parse_from_str(&params.end_date, "%Y-%m-%d")
        .unwrap_or_else(|_| Utc::now().date_naive())
        .and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())
        .and_utc();

    debug!("Time window: {} to {}", start_cutoff, end_cutoff);

    if (end_cutoff - start_cutoff).num_days() > 30 {
        let template = IndexTemplate {
            daily_groups: vec![],
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

    // 2. Update Mapping
    {
        let mut map_guard = state.character_map.lock().unwrap();
        map_guard.clear();
        for line in params.mapping_input.lines() {
            if let Some((alt, main)) = line.split_once([':', '=']) {
                map_guard.insert(alt.trim().to_string(), main.trim().to_string());
            }
        }
    }

    // 3. Fetch Data
    let fetch_result = if !params.zkill_link.is_empty() {
        Some(fetch_zkill_data(&params.zkill_link, &state, start_cutoff).await)
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

    let excluded_ids: HashSet<i32> = params
        .excluded_kills
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    let excluded_names: HashSet<String> = params
        .excluded_beneficiaries
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // 4. Filter Active Kills
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

    debug!("Active kills in range: {}", final_kills.len());

    // 5. Calculate Payout
    let current_map = state.character_map.lock().unwrap().clone();
    let mut all_seen_mains: HashSet<String> = HashSet::new();
    let mut main_wallets: HashMap<String, f64> = HashMap::new();
    let mut total_dropped_value = 0.0;

    for kill in &final_kills {
        if !kill.is_active {
            continue;
        }

        total_dropped_value += kill.zkb.dropped_value;

        let mut kill_participants: HashSet<String> = HashSet::new();
        for attacker in &kill.attackers {
            if let Some(name) = &attacker.character_name {
                let main = current_map.get(name).unwrap_or(name);
                all_seen_mains.insert(main.clone());
                if !excluded_names.contains(main) {
                    kill_participants.insert(main.clone());
                }
            }
        }

        if kill_participants.is_empty() {
            continue;
        }

        let participant_count = kill_participants.len() as f64;
        let share_per_pilot = kill.zkb.dropped_value / participant_count;

        for main in kill_participants {
            *main_wallets.entry(main).or_insert(0.0) += share_per_pilot;
        }
    }

    // 6. Beneficiaries List
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

    // 7. Grouping
    let mut groups_map: HashMap<String, Vec<Killmail>> = HashMap::new();
    for kill in final_kills {
        let date_str = kill
            .killmail_time
            .split('T')
            .next()
            .unwrap_or("Unknown")
            .to_string();
        groups_map.entry(date_str).or_default().push(kill);
    }

    let mut daily_groups = Vec::new();
    let mut dates: Vec<String> = groups_map.keys().cloned().collect();
    dates.sort_by(|a, b| b.cmp(a));

    for date in dates {
        if let Some(kills) = groups_map.remove(&date) {
            daily_groups.push(DailyGroup {
                date_display: date,
                kills,
            });
        }
    }

    let template = IndexTemplate {
        daily_groups,
        mapping_text: params.mapping_input,
        zkill_link: params.zkill_link,
        start_date: params.start_date,
        end_date: params.end_date,
        total_payout_str: format_isk(total_dropped_value),
        total_humans: active_humans,
        beneficiaries,
        error_msg,
    };

    Html(template.render().unwrap())
}
