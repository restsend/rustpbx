mod common;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/helpers.rs"]
mod wholesale_helpers;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/billing_service_test.rs"]
mod billing_service_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/billing_test.rs"]
mod billing_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/caller_pool_test.rs"]
mod caller_pool_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/data_test.rs"]
mod data_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/export_test.rs"]
mod export_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/flow.rs"]
mod flow;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/handlers_test.rs"]
mod handlers_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/multi_trunk_test.rs"]
mod multi_trunk_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/optimization_test.rs"]
mod optimization_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/rate_limit_test.rs"]
mod rate_limit_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/retry_test.rs"]
mod retry_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/route_test.rs"]
mod route_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/stats_test.rs"]
mod stats_test;

#[cfg(feature = "addon-wholesale")]
#[path = "wholesale/template_check.rs"]
mod template_check;
