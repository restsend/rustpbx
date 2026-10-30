#[path = "queue_e2e/test_queue_routing.rs"]
mod test_queue_routing;

#[path = "queue_e2e/test_queue_hold_audio.rs"]
mod test_queue_hold_audio;














#[path = "queue_e2e/test_blind_transfer_customer_e2e.rs"]
#[cfg(feature = "addon-cc")]
mod test_blind_transfer_customer_e2e;

#[path = "queue_e2e/test_agent_acw_status_e2e.rs"]
#[cfg(feature = "addon-cc")]
mod test_agent_acw_status_e2e;

#[path = "queue_e2e/test_agent_event_contract_e2e.rs"]
#[cfg(feature = "addon-cc")]
mod test_agent_event_contract_e2e;
