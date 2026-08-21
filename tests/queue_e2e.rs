mod common;

#[path = "queue_e2e/test_queue_routing.rs"]
mod test_queue_routing;

#[path = "queue_e2e/test_queue_concurrent.rs"]
#[cfg(feature = "addon-cc")]
mod test_queue_concurrent;

#[cfg(feature = "addon-cc")]
#[path = "queue_e2e/test_queue_escalation_e2e.rs"]
mod test_queue_escalation_e2e;
