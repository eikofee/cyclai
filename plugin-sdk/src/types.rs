use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// `GET /manifest`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub protocol: u32,
    /// Stable id, lowercase letters and digits (`garmin`): stored with
    /// every activity it brings.
    pub id: String,
    /// Name shown in the settings page.
    pub label: String,
    pub version: String,
    /// Sports it can bring (`Ride`, `VirtualRide`, `Run`, `TrailRun`).
    #[serde(default)]
    pub sports: Vec<String>,
    /// False while credentials are missing; CyclAI then does not sync it.
    pub configured: bool,
    /// Why it is not configured, or any status worth showing.
    #[serde(default)]
    pub message: Option<String>,
    /// Link to an activity on the source's own site, `{id}` standing for the
    /// activity's `external_id` (e.g. `https://example.com/activities/{id}`).
    #[serde(default)]
    pub activity_url: Option<String>,
    /// Label of that link ("voir sur Garmin Connect").
    #[serde(default)]
    pub activity_url_label: Option<String>,
}

/// An activity as announced by the listing, before anything is downloaded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListedActivity {
    pub external_id: String,
    pub title: String,
    /// RFC 3339, UTC.
    pub started_at: String,
    pub start_epoch: i64,
    /// Known from the listing for some sources: lets CyclAI skip an activity
    /// that overlaps another before downloading it.
    #[serde(default)]
    pub duration_s: Option<i64>,
    pub sport_type: String,
}

/// `GET /activities`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ListResponse {
    pub activities: Vec<ListedActivity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lap {
    pub lap_index: i64,
    pub elapsed_time_s: i64,
    pub distance_m: f64,
    #[serde(default)]
    pub avg_power_w: Option<f64>,
    #[serde(default)]
    pub avg_hr: Option<f64>,
    #[serde(default)]
    pub max_hr: Option<f64>,
}

/// A complete activity. `streams` follows the layout CyclAI stores
/// (`{"time": {"data": [...]}, "watts": {...}, "heartrate": {...},
/// "latlng": {...}, "altitude": {...}, "distance": {...}, "cadence": {...}}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub started_at: String,
    pub sport_type: String,
    pub duration_s: i64,
    pub distance_m: f64,
    pub elevation_gain_m: f64,
    #[serde(default)]
    pub avg_power_w: Option<f64>,
    #[serde(default)]
    pub normalized_power_w: Option<f64>,
    #[serde(default)]
    pub avg_hr: Option<f64>,
    #[serde(default)]
    pub max_hr: Option<f64>,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub streams: Value,
    #[serde(default)]
    pub laps: Vec<Lap>,
}

/// What `GET /activities/{id}` gives back.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    Activity(Activity),
    /// The original `.fit` file; CyclAI parses it, the plugin stays light.
    Fit(Vec<u8>),
    /// Nothing to import for this id.
    NotAvailable,
}

/// A failure a plugin reports to CyclAI.
#[derive(Debug, Clone)]
pub struct PluginError {
    /// HTTP status sent back: 429 for a rate limit, 502 for an upstream failure.
    pub status: u16,
    pub message: String,
    /// Sent as `Retry-After` for a 429.
    pub retry_after_s: Option<u64>,
}

impl PluginError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), retry_after_s: None }
    }
    /// The source (Garmin...) failed or refused.
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(502, message)
    }
    pub fn rate_limited(message: impl Into<String>, retry_after_s: Option<u64>) -> Self {
        Self { status: 429, message: message.into(), retry_after_s }
    }
    pub fn not_configured(message: impl Into<String>) -> Self {
        Self::new(503, message)
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PluginError {}

impl From<anyhow::Error> for PluginError {
    fn from(e: anyhow::Error) -> Self {
        Self::upstream(format!("{e:#}"))
    }
}

/// What a plugin implements; [`crate::serve`] turns it into the HTTP service.
#[async_trait]
pub trait Plugin: Send + Sync {
    async fn manifest(&self) -> Manifest;
    /// Activities started after `after_epoch` (0 = the whole history).
    async fn list(&self, after_epoch: i64) -> Result<Vec<ListedActivity>, PluginError>;
    async fn fetch(&self, external_id: &str) -> Result<Fetched, PluginError>;
}
