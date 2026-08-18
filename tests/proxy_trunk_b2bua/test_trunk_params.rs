use anyhow::Result;
use rustpbx::config::{MediaProxyMode, ProxyConfig};
use rustpbx::proxy::routing::TrunkConfig;
use std::collections::HashMap;
use std::sync::Arc;

use crate::common::e2e_test_server::E2eTestServer;

fn trunk_config_with(trunk: TrunkConfig) -> ProxyConfig {
    let mut trunks = HashMap::new();
    trunks.insert("test_trunk".to_string(), trunk);
    ProxyConfig {
        media_proxy: MediaProxyMode::All,
        trunks,
        ..Default::default()
    }
}

#[tokio::test]
async fn test_disabled_trunk_not_routed() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let config = trunk_config_with(TrunkConfig {
        dest: "sip:127.0.0.1:5099".to_string(),
        disabled: Some(true),
        ..Default::default()
    });
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let trunks = server.server_ref.data_context.trunks_snapshot();
    let trunk = trunks.get("test_trunk").unwrap();
    assert_eq!(trunk.disabled, Some(true), "disabled flag should be set");

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_trunk_max_calls_loaded() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let config = trunk_config_with(TrunkConfig {
        dest: "sip:127.0.0.1:5099".to_string(),
        max_calls: Some(5),
        ..Default::default()
    });
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let trunks = server.server_ref.data_context.trunks_snapshot();
    let trunk = trunks.get("test_trunk").unwrap();
    assert_eq!(trunk.max_calls, Some(5), "max_calls should be set");
    assert!(
        trunk.concurrent_call_limiter.is_some(),
        "concurrent_call_limiter should be built from max_calls"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_trunk_max_cps_loaded() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let config = trunk_config_with(TrunkConfig {
        dest: "sip:127.0.0.1:5099".to_string(),
        max_cps: Some(10),
        ..Default::default()
    });
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let trunks = server.server_ref.data_context.trunks_snapshot();
    let trunk = trunks.get("test_trunk").unwrap();
    assert_eq!(trunk.max_cps, Some(10), "max_cps should be set");
    assert!(
        trunk.cps_limiter.is_some(),
        "cps_limiter should be built from max_cps"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_trunk_backup_dest_loaded() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let config = trunk_config_with(TrunkConfig {
        dest: "sip:127.0.0.1:5099".to_string(),
        backup_dest: Some("sip:127.0.0.1:5098".to_string()),
        ..Default::default()
    });
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let trunks = server.server_ref.data_context.trunks_snapshot();
    let trunk = trunks.get("test_trunk").unwrap();
    assert_eq!(
        trunk.backup_dest.as_deref(),
        Some("sip:127.0.0.1:5098"),
        "backup_dest should be set"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_trunk_register_headers_loaded() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let mut extra_headers = HashMap::new();
    extra_headers.insert("X-Custom".to_string(), "custom-value".to_string());

    let config = trunk_config_with(TrunkConfig {
        dest: "sip:127.0.0.1:5099".to_string(),
        register_enabled: Some(true),
        register_expires: Some(120),
        register_extra_headers: Some(extra_headers),
        ..Default::default()
    });
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let trunks = server.server_ref.data_context.trunks_snapshot();
    let trunk = trunks.get("test_trunk").unwrap();
    assert_eq!(trunk.register_enabled, Some(true));
    assert_eq!(trunk.register_expires, Some(120));
    assert!(
        trunk
            .register_extra_headers
            .as_ref()
            .unwrap()
            .contains_key("X-Custom"),
        "register_extra_headers should be set"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_trunk_max_ring_time_loaded() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let config = trunk_config_with(TrunkConfig {
        dest: "sip:127.0.0.1:5099".to_string(),
        max_ring_time: Some(30),
        ..Default::default()
    });
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let trunks = server.server_ref.data_context.trunks_snapshot();
    let trunk = trunks.get("test_trunk").unwrap();
    assert_eq!(trunk.max_ring_time, Some(30), "max_ring_time should be set");

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_trunk_call_id_mode_loaded() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let config = trunk_config_with(TrunkConfig {
        dest: "sip:127.0.0.1:5099".to_string(),
        call_id_mode: Some(rustpbx::proxy::routing::CallIdMode::Rewrite),
        ..Default::default()
    });
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let trunks = server.server_ref.data_context.trunks_snapshot();
    let trunk = trunks.get("test_trunk").unwrap();
    assert_eq!(
        trunk.call_id_mode,
        Some(rustpbx::proxy::routing::CallIdMode::Rewrite),
        "call_id_mode should be set"
    );

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_trunk_weight_loaded() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let config = trunk_config_with(TrunkConfig {
        dest: "sip:127.0.0.1:5099".to_string(),
        weight: Some(3),
        ..Default::default()
    });
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let trunks = server.server_ref.data_context.trunks_snapshot();
    let trunk = trunks.get("test_trunk").unwrap();
    assert_eq!(trunk.weight, Some(3), "weight should be set");

    server.stop();
    Ok(())
}

#[tokio::test]
async fn test_trunk_incoming_prefixes_loaded() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let config = trunk_config_with(TrunkConfig {
        dest: "sip:127.0.0.1:5099".to_string(),
        incoming_from_user_prefix: Some("^\\+1".to_string()),
        incoming_to_user_prefix: Some("^9".to_string()),
        ..Default::default()
    });
    let server = Arc::new(E2eTestServer::start_with_config(config).await?);

    let trunks = server.server_ref.data_context.trunks_snapshot();
    let trunk = trunks.get("test_trunk").unwrap();
    assert_eq!(trunk.incoming_from_user_prefix.as_deref(), Some("^\\+1"));
    assert_eq!(trunk.incoming_to_user_prefix.as_deref(), Some("^9"));

    server.stop();
    Ok(())
}
