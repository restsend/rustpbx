mod common;

#[path = "queue_e2e/test_queue_routing.rs"]
mod test_queue_routing;

#[path = "queue_e2e/test_queue_concurrent.rs"]
#[cfg(feature = "addon-cc")]
mod test_queue_concurrent;
