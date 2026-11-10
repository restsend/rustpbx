//! External realtime quality service: PCM input and text/trace/hangup output.
use serde::{Deserialize, Serialize};

/// Select logical call legs, independent of agent/customer identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QualitySides {
    Caller,
    Callee,
    #[default]
    Both,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QualityConfig {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub parameters: serde_json::Map<String, serde_json::Value>,
    /// Start inspection automatically after answer when enabled.
    pub enabled: bool,
    pub sides: QualitySides,
    /// Maximum seconds without a message from the quality service.
    pub receive_timeout_secs: u64,
}

impl Default for QualityConfig {
    fn default() -> Self {
        Self {
            url: String::new(), headers: Vec::new(), parameters: Default::default(),
            enabled: false, sides: QualitySides::default(), receive_timeout_secs: 15,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QualityPlan {
    pub routing: QualityRoutingContext,
    pub enabled: Option<bool>,
    pub sides: Option<QualitySides>,
    pub receive_timeout_secs: Option<u64>,
    pub url: Option<String>,
    pub headers: Option<Vec<(String, String)>>,
    pub parameters: serde_json::Map<String, serde_json::Value>,
}
impl QualityConfig {
    pub fn apply(&mut self, plan: &QualityPlan) {
        if let Some(sides) = plan.sides { self.sides = sides; }
        if let Some(seconds) = plan.receive_timeout_secs { self.receive_timeout_secs = seconds; }
        if let Some(url) = &plan.url { self.url = url.clone(); }
        if let Some(headers) = &plan.headers { self.headers = headers.clone(); }
        self.parameters.extend(plan.parameters.clone());
        if let Some(enabled) = plan.enabled { self.enabled = enabled; }
    }
}

/// Trusted routing identifiers supplied by the routing addon, never ASR parameters.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QualityRoutingContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inbound_trunk_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbound_trunk_id: Option<i64>,
}

/// Which call participant produced the audio for a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QualitySide {
    Caller,
    Callee,
}

impl QualitySide {
    pub fn as_str(&self) -> &'static str {
        match self {
            QualitySide::Caller => "caller",
            QualitySide::Callee => "callee",
        }
    }

    pub fn from_leg_side(side: crate::media::media_bridge::LegSide) -> Self {
        match side {
            crate::media::media_bridge::LegSide::A => QualitySide::Caller,
            crate::media::media_bridge::LegSide::B => QualitySide::Callee,
        }
    }
}

/// Text returned for the single audio track on a quality connection.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TranscriptSegment {
    /// Recognized text. May be refined across consecutive partials of the
    /// same utterance; the final segment (`partial == false`) is definitive.
    pub text: String,
    /// `true` for interim hypotheses, `false` for the finalized utterance.
    #[serde(default)]
    pub partial: bool,
    /// Offset from transcription start, in milliseconds.
    pub start_ms: u64,
    /// Offset from transcription start, in milliseconds.
    pub end_ms: u64,
    /// Detected / configured language, when known.
    pub lang: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receive_timeout_defaults_and_overrides() {
        assert_eq!(QualityConfig::default().receive_timeout_secs, 15);
        let mut config: QualityConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.receive_timeout_secs, 15);
        let plan: QualityPlan = serde_json::from_str(r#"{"receive_timeout_secs":30}"#).unwrap();
        config.apply(&plan);
        assert_eq!(config.receive_timeout_secs, 30);
        config.apply(&QualityPlan::default());
        assert_eq!(config.receive_timeout_secs, 30);
    }

    #[test]
    fn enable_policy_inherits_unless_explicitly_overridden() {
        let mut config = QualityConfig::default();
        config.apply(&QualityPlan { url: Some("ws://tenant/quality".into()), ..Default::default() });
        assert!(!config.enabled, "endpoint-only overrides must not enable inspection");
        config.apply(&QualityPlan { enabled: Some(true), ..Default::default() });
        assert!(config.enabled);
        config.apply(&QualityPlan::default());
        assert!(config.enabled);
        config.apply(&QualityPlan { enabled: Some(false), ..Default::default() });
        assert!(!config.enabled);
        for retired in ["auto_start", "allow_hangup", "text_enabled"] {
            let mut value = serde_json::json!({"enabled":true});
            value[retired] = true.into();
            assert!(serde_json::from_value::<QualityConfig>(value).is_err());
        }
    }
}
