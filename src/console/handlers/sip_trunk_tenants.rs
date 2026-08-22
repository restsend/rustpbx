//! SIP-trunk ↔ tenant association via the addon registry (no addon model imports).

use crate::console::ConsoleState;
use serde_json::Value;
use tracing::warn;

fn registry(
    state: &ConsoleState,
) -> Option<std::sync::Arc<crate::addons::registry::AddonRegistry>> {
    state.app_state().map(|app| app.addon_registry.clone())
}

pub async fn load_tenants(state: &ConsoleState) -> Vec<Value> {
    let Some(reg) = registry(state) else {
        return vec![];
    };
    reg.list_trunk_tenants(state.db()).await
}

pub async fn get_trunk_tenant_id(state: &ConsoleState, trunk_id: i64) -> Option<i64> {
    let Some(reg) = registry(state) else {
        return None;
    };
    reg.get_trunk_tenant_id(state.db(), trunk_id).await
}

pub async fn handle_tenant_update(
    state: &ConsoleState,
    trunk_id: i64,
    tenant_id: Option<i64>,
    clear_tenant: bool,
) -> Result<(), sea_orm::DbErr> {
    let Some(reg) = registry(state) else {
        return Ok(());
    };
    if let Err(err) = reg
        .set_trunk_tenant(state.db(), trunk_id, tenant_id, clear_tenant)
        .await
    {
        warn!("failed to update trunk tenant link: {}", err);
        return Err(err);
    }
    Ok(())
}
