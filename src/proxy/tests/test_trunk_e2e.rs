//! Trunk Routing E2E Tests
//!
//! Verifies trunk and route configuration injection into E2eTestServer.
//!
//! These tests verify that:
//! - Custom ProxyConfig with trunks/routes can be injected
//! - Trunk data is loaded into DataContext correctly
//! - Route data is loaded into DataContext correctly
//! - Multiple trunks are all available
//!
//! Note: Full call-flow tests through trunk routes require a separate
//! trunk destination server, which is beyond the scope of these unit tests.
//! The existing wholesale E2E tests cover the actual call flow path.

use crate::config::{MediaProxyMode, ProxyConfig};
use crate::proxy::routing::{DestConfig, MatchConditions, RouteAction, RouteRule, TrunkConfig};
use anyhow::Result;
use std::collections::HashMap;

fn trunk_test_proxy_config() -> ProxyConfig {
    let mut trunks = HashMap::new();

    trunks.insert(
        "provider_a".to_string(),
        TrunkConfig {
            dest: "sip:carrier.example.com:5060".to_string(),
            inbound_hosts: vec!["203.0.113.10".to_string()],
            ..Default::default()
        },
    );

    trunks.insert(
        "provider_b".to_string(),
        TrunkConfig {
            dest: "sip:backup.example.com:5060".to_string(),
            inbound_hosts: vec!["203.0.113.20".to_string()],
            ..Default::default()
        },
    );

    let mut routes = vec![RouteRule {
        name: "route_national_via_provider_a".to_string(),
        description: Some("Route national calls via provider_a".to_string()),
        priority: 1,
        match_conditions: MatchConditions {
            to_user: Some("^0[2-9]\\d{8}$".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("forward".to_string()),
            dest: Some(DestConfig::Single("provider_a".to_string())),
            ..Default::default()
        },
        ..Default::default()
    }];

    routes.push(RouteRule {
        name: "route_international_via_provider_b".to_string(),
        description: Some("Route international calls via provider_b".to_string()),
        priority: 10,
        match_conditions: MatchConditions {
            to_user: Some("^00\\d+".to_string()),
            ..Default::default()
        },
        action: RouteAction {
            action: Some("forward".to_string()),
            dest: Some(DestConfig::Single("provider_b".to_string())),
            ..Default::default()
        },
        ..Default::default()
    });

    ProxyConfig {
        media_proxy: MediaProxyMode::All,
        trunks,
        routes: Some(routes),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::tests::e2e_test_server::E2eTestServer;

    /// Test that a server with trunk config injection starts correctly
    /// and all trunks are available in the data context.
    #[tokio::test]
    async fn test_trunk_config_injection_server_starts() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();

        let config = trunk_test_proxy_config();
        let server = E2eTestServer::start_with_config(config).await?;

        // Verify trunk data is loaded
        let trunks = server.server_ref.data_context.trunks_snapshot();
        assert!(
            trunks.contains_key("provider_a"),
            "Should have provider_a trunk"
        );
        assert!(
            trunks.contains_key("provider_b"),
            "Should have provider_b trunk"
        );

        let provider_a = &trunks["provider_a"];
        assert_eq!(provider_a.dest, "sip:carrier.example.com:5060");
        assert!(
            provider_a
                .inbound_hosts
                .contains(&"203.0.113.10".to_string())
        );

        let provider_b = &trunks["provider_b"];
        assert_eq!(provider_b.dest, "sip:backup.example.com:5060");
        assert!(
            provider_b
                .inbound_hosts
                .contains(&"203.0.113.20".to_string())
        );

        server.stop();
        Ok(())
    }

    /// Test that route data is loaded correctly.
    #[tokio::test]
    async fn test_trunk_route_config_loaded() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();

        let config = trunk_test_proxy_config();
        let server = E2eTestServer::start_with_config(config).await?;

        let routes = server.server_ref.data_context.routes_snapshot();
        assert_eq!(routes.len(), 2, "Should have 2 routes configured");

        // Routes should be sorted by priority
        assert_eq!(routes[0].name, "route_national_via_provider_a");
        assert_eq!(routes[0].priority, 1);
        assert_eq!(routes[1].name, "route_international_via_provider_b");
        assert_eq!(routes[1].priority, 10);

        server.stop();
        Ok(())
    }

    /// Test that multiple trunks are all available.
    #[tokio::test]
    async fn test_trunk_snapshot_multiple_trunks() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();

        let mut config = trunk_test_proxy_config();

        // Add more trunks
        for i in 3..=5 {
            config.trunks.insert(
                format!("provider_{}", i),
                TrunkConfig {
                    dest: format!("sip:10.0.0.{}:5060", i),
                    inbound_hosts: vec![format!("10.0.0.{}", i)],
                    ..Default::default()
                },
            );
        }

        let server = E2eTestServer::start_with_config(config).await?;

        let trunks = server.server_ref.data_context.trunks_snapshot();
        assert_eq!(trunks.len(), 5, "Should have 5 trunks configured");

        for name in &[
            "provider_a",
            "provider_b",
            "provider_3",
            "provider_4",
            "provider_5",
        ] {
            assert!(trunks.contains_key(*name), "Missing trunk: {}", name);
        }

        server.stop();
        Ok(())
    }

    /// Test that trunk inbound_hosts matching works.
    #[tokio::test]
    async fn test_trunk_inbound_ip_matching() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();

        let config = trunk_test_proxy_config();
        let server = E2eTestServer::start_with_config(config).await?;

        let trunks = server.server_ref.data_context.trunks_snapshot();

        let provider_a = &trunks["provider_a"];
        let matching_ip: std::net::IpAddr = "203.0.113.10".parse().unwrap();
        assert!(provider_a.matches_inbound_ip(&matching_ip).await);

        let non_matching_ip: std::net::IpAddr = "10.0.0.1".parse().unwrap();
        assert!(!provider_a.matches_inbound_ip(&non_matching_ip).await);

        server.stop();
        Ok(())
    }

    /// Test route with multiple destinations (load balancing).
    #[tokio::test]
    async fn test_trunk_route_multiple_destinations() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();

        let mut config = trunk_test_proxy_config();

        // Add a route with multiple destinations
        config.routes.as_mut().unwrap().push(RouteRule {
            name: "route_premium_load_balanced".to_string(),
            description: Some("Premium route load balanced across providers".to_string()),
            priority: 5,
            match_conditions: MatchConditions {
                to_user: Some("^1[3-9]\\d{9}$".to_string()),
                ..Default::default()
            },
            action: RouteAction {
                action: Some("forward".to_string()),
                dest: Some(DestConfig::Multiple(vec![
                    "provider_a".to_string(),
                    "provider_b".to_string(),
                ])),
                ..Default::default()
            },
            ..Default::default()
        });

        let server = E2eTestServer::start_with_config(config).await?;

        let routes = server.server_ref.data_context.routes_snapshot();
        assert_eq!(routes.len(), 3, "Should have 3 routes");

        let premium_route = routes
            .iter()
            .find(|r| r.name == "route_premium_load_balanced")
            .unwrap();
        match &premium_route.action.dest {
            Some(DestConfig::Multiple(dests)) => {
                assert_eq!(dests.len(), 2);
                assert!(dests.contains(&"provider_a".to_string()));
                assert!(dests.contains(&"provider_b".to_string()));
            }
            _ => panic!("Expected Multiple destinations"),
        }

        server.stop();
        Ok(())
    }

    /// Test that a P2P call still works when trunk config is present
    /// (trunks/routes don't break normal user-to-user calls).
    #[tokio::test]
    async fn test_trunk_config_p2p_call_still_works() -> Result<()> {
        let _ = tracing_subscriber::fmt::try_init();

        let config = trunk_test_proxy_config();
        let server = std::sync::Arc::new(E2eTestServer::start_with_config(config).await?);

        // Normal P2P call between registered users (not matching any route)
        let alice = std::sync::Arc::new(server.create_ua("alice").await?);
        let bob = server.create_ua("bob").await?;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let _sdp = super::super::rtp_utils::RtpPacket::create_sequence(
            50, 1000, 50000, 0xA1A1A1A1, 0, 160, 160,
        );
        let _caller_sdp = "v=0\r\no=- 1234 1234 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 12345 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=sendrecv\r\n".to_string();

        let alice_clone = alice.clone();
        let caller_sdp = _caller_sdp.clone();
        let caller_handle =
            tokio::spawn(async move { alice_clone.make_call("bob", Some(caller_sdp)).await });

        // Bob answers
        let mut bob_dialog_id = None;
        for _ in 0..50 {
            let events = bob.process_dialog_events().await?;
            for event in events {
                if let super::super::test_ua::TestUaEvent::IncomingCall(id, _) = event {
                    bob_dialog_id = Some(id.clone());
                    bob.answer_call(&id, Some(_caller_sdp.clone())).await?;
                    break;
                }
            }
            if bob_dialog_id.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let alice_id = tokio::time::timeout(std::time::Duration::from_secs(5), caller_handle)
            .await
            .map_err(|_| anyhow::anyhow!("timeout"))?
            .map_err(|e| anyhow::anyhow!("join: {}", e))?
            .map_err(|e| anyhow::anyhow!("call: {}", e))?;

        // Call established - hangup
        alice.hangup(&alice_id).await?;

        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        let records = server.cdr_capture.get_all_records().await;
        assert!(!records.is_empty(), "Should have CDR");

        let record = &records[0];
        assert_eq!(record.details.status, "completed");

        server.stop();
        Ok(())
    }
}
