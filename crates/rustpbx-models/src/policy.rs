use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct PolicySpec {
    #[serde(default)]
    pub called_prefix: Option<String>,
    #[serde(default)]
    pub trunk_country: Option<String>,
    #[serde(default)]
    pub allowed_destination_countries: Vec<String>,
    #[serde(default)]
    pub time_window: Option<TimeWindow>,
    #[serde(default)]
    pub deny_regions: Vec<String>,
    #[serde(default)]
    pub allow_landline: Option<bool>,
    #[serde(default)]
    pub frequency_limit: Option<FrequencyLimit>,
    #[serde(default)]
    pub daily_limit: Option<DailyLimit>,
    #[serde(default)]
    pub concurrency: Option<ConcurrencyLimit>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TimeWindow {
    pub start: String,
    pub end: String,
    pub timezone: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct FrequencyLimit {
    pub count: u32,
    pub window_hours: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DailyLimit {
    pub count: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConcurrencyLimit {
    pub max_total: u32,
    #[serde(default)]
    pub max_per_account: HashMap<String, u32>,
}
