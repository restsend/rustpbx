mod common;

#[cfg(feature = "addon-cc")]
#[path = "cc_e2e/skill_group_routing_tests.rs"]
mod skill_group_routing_tests;

#[cfg(feature = "addon-cc")]
#[path = "cc_e2e/skill_groups_config_tests.rs"]
mod skill_groups_config_tests;

#[cfg(feature = "addon-cc")]
#[path = "cc_e2e/acd_e2e_test.rs"]
mod acd_e2e_test;

#[cfg(feature = "addon-cc")]
#[path = "cc_e2e/cluster_e2e_tests.rs"]
mod cluster_e2e_tests;

#[cfg(feature = "addon-cc")]
#[path = "cc_e2e/cc_agent_events_e2e_test.rs"]
mod cc_agent_events_e2e_test;

#[cfg(feature = "addon-cc")]
#[path = "cc_e2e/test_consult_transfer.rs"]
mod test_consult_transfer;

#[cfg(feature = "addon-cc")]
#[path = "cc_e2e/test_hold_unhold_e2e.rs"]
mod test_hold_unhold_e2e;

#[cfg(feature = "addon-cc")]
#[path = "cc_e2e/webhook_agent_events_e2e_test.rs"]
mod webhook_agent_events_e2e_test;

