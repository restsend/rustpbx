//! Dedicated contract binary: step-IVR `ivr_step_trace` + lifecycle events
//! over the REAL RWI webhook path.
//!
//! Kept as its own `[[test]]` target (same pattern as
//! `cc_openapi_contract_test`) so this regression guard stays runnable —
//! and breakage-visible — independently of the aggregated `e2e_core`
//! suite's feature matrix.
//!
//! `mod common;` declared here once; suite modules reach it via
//! `crate::common`.

mod common;

#[path = "ivr_e2e/test_step_ivr_trace_webhook_e2e.rs"]
mod test_step_ivr_trace_webhook_e2e;
#[path = "ivr_e2e/test_inband_dtmf_e2e.rs"]
mod test_inband_dtmf_e2e;
