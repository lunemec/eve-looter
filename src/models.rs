use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

// --- Helper: Human Readable ISK ---
pub fn format_isk(amount: f64) -> String {
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

// --- App State ---
pub struct AppState {
    pub current_kills: Mutex<Vec<Killmail>>,
    pub character_map: Mutex<HashMap<String, String>>,
    pub esi_cache: Mutex<HashMap<i32, EsiKillmail>>,
    pub name_cache: Mutex<HashMap<i32, String>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            current_kills: Mutex::new(Vec::new()),
            character_map: Mutex::new(HashMap::new()),
            esi_cache: Mutex::new(HashMap::new()),
            name_cache: Mutex::new(HashMap::new()),
        }
    }
}

// --- Main Domain Objects ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Killmail {
    pub killmail_id: i32,
    pub zkb: ZkbStats,
    pub victim: Option<Victim>,
    pub attackers: Vec<Attacker>,
    pub killmail_time: String,
    pub formatted_dropped: String,
    #[serde(default = "default_true")]
    pub is_active: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZkbStats {
    #[serde(rename = "locationID")]
    pub location_id: i32,
    pub hash: String,
    #[serde(rename = "fittedValue")]
    pub fitted_value: f64,
    #[serde(rename = "droppedValue")]
    pub dropped_value: f64,
    #[serde(rename = "destroyedValue")]
    pub destroyed_value: f64,
    #[serde(rename = "totalValue")]
    pub total_value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Victim {
    pub character_id: Option<i32>,
    pub character_name: Option<String>,
    pub corporation_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attacker {
    pub character_id: Option<i32>,
    pub character_name: Option<String>,
    pub corporation_id: Option<i32>,
}

// --- Fetching / ESI Intermediate Structs ---

#[derive(Debug, Clone, Deserialize)]
pub struct RawZKillItem {
    pub killmail_id: i32,
    pub zkb: ZkbStats,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EsiKillmail {
    pub killmail_time: String,
    pub victim: EsiVictim,
    pub attackers: Vec<EsiAttacker>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EsiVictim {
    pub character_id: Option<i32>,
    pub corporation_id: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EsiAttacker {
    pub character_id: Option<i32>,
    pub corporation_id: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EsiNameEntry {
    pub id: i32,
    pub name: String,
    #[allow(dead_code)]
    pub category: String,
}
