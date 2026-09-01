mod common;

#[path = "queue_e2e/test_queue_routing.rs"]
mod test_queue_routing;

#[path = "queue_e2e/test_queue_concurrent.rs"]
#[cfg(feature = "addon-cc")]
mod test_queue_concurrent;

#[path = "queue_e2e/test_queue_escalation_e2e.rs"]
#[cfg(feature = "addon-cc")]
mod test_queue_escalation_e2e;

#[path = "queue_e2e/test_queue_wait_retention_e2e.rs"]
#[cfg(feature = "addon-cc")]
mod test_queue_wait_retention_e2e;

#[path = "queue_e2e/test_ivr_queue_agent_full_rwi_e2e.rs"]
#[cfg(feature = "addon-cc")]
mod test_ivr_queue_agent_full_rwi_e2e;

#[path = "queue_e2e/test_queue_overflow_uri_override_e2e.rs"]
#[cfg(feature = "addon-cc")]
mod test_queue_overflow_uri_override_e2e;
