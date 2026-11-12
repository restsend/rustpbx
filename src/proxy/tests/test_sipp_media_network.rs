//! Two real SIPp endpoints exercise inbound routing and both media SDP legs.
use crate::config::{MediaProxyMode, NetworkProfile, ProxyConfig, RtpConfig};
use crate::proxy::acl::AclModule;
use crate::proxy::call::CallModule;
use crate::proxy::locator::MemoryLocator;
use crate::proxy::routing::{DestConfig, MediaMode, RouteAction, RouteRule, TrunkConfig, TrunkDirection};
use crate::proxy::server::SipServerBuilder;
use crate::proxy::user::MemoryUserBackend;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// SIPP_BIN=/path/to/sipp cargo test --lib sipp_two_legs_media_network -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires SIPp and a non-loopback local IPv4 interface"]
async fn sipp_two_legs_media_network() {
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer().try_init();
    }
    let local = rustrtc::transports::get_local_ip().unwrap().to_string();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for (case, a_bind, a_ip, b_bind, b_ip) in [
        ("localhost", "127.0.0.1", "127.0.0.1", "127.0.0.1", "127.0.0.1"),
        ("lan_and_localhost", local.as_str(), local.as_str(), "127.0.0.1", "127.0.0.1"),
        ("distinct_advertised", local.as_str(), "203.0.113.10", local.as_str(), "198.51.100.20"),
    ] {
        let logs = root.join("target/sipp-media-network").join(case);
        std::fs::create_dir_all(&logs).unwrap();
        let reservations: Vec<_> = (0..3).map(|_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap()).collect();
        let ports: Vec<_> = reservations.iter().map(|s| s.local_addr().unwrap().port()).collect();
        let (pbx_port, uac_port, uas_port) = (ports[0], ports[1], ports[2]);
        let profiles: Vec<NetworkProfile> = serde_json::from_value(serde_json::json!([
            { "id": "wan", "bind_ip": a_bind, "external_ip": a_ip,
              "sip_external_ip": "127.0.0.1", "sip_contact_always_bind": true,
              "rtp_start_port": 20000, "rtp_end_port": 24998 },
            { "id": "ims", "bind_ip": b_bind, "external_ip": b_ip,
              "sip_external_ip": "127.0.0.1", "sip_contact_always_bind": true,
              "rtp_start_port": 25000, "rtp_end_port": 29998 }
        ])).unwrap();
        std::fs::write(logs.join("network-profiles.json"), serde_json::to_string_pretty(&profiles).unwrap()).unwrap();
        let mut config = ProxyConfig {
            addr: "127.0.0.1".into(), udp_port: Some(pbx_port), tcp_port: None,
            modules: Some(vec!["acl".into(), "call".into()]),
            ensure_user: Some(false), media_proxy: MediaProxyMode::All,
            acl_rules: Some(vec!["allow 127.0.0.1".into(), "deny all".into()]),
            generated_dir: logs.join("generated").to_string_lossy().into_owned(),
            ..Default::default()
        };
        config.trunks.insert("inbound".into(), TrunkConfig {
            direction: Some(TrunkDirection::Inbound), inbound_hosts: vec!["127.0.0.1".into()],
            profile: Some("wan".into()), media_mode: Some(MediaMode::ForceTranscode),
            ..Default::default()
        });
        config.trunks.insert("outbound".into(), TrunkConfig {
            direction: Some(TrunkDirection::Outbound), dest: format!("sip:127.0.0.1:{uas_port}"),
            profile: Some("ims".into()), media_mode: Some(MediaMode::ForceTranscode),
            ..Default::default()
        });
        config.routes = Some(vec![RouteRule {
            name: "sipp-two-legs".into(), source_trunks: vec!["inbound".into()],
            action: RouteAction { dest: Some(DestConfig::Single("outbound".into())), ..Default::default() },
            ..Default::default()
        }]);
        let token = CancellationToken::new();
        let _cancel = token.clone().drop_guard();
        drop(reservations);
        let server = SipServerBuilder::new(Arc::new(config))
            .with_cancel_token(token.clone())
            .with_user_backend(Box::new(MemoryUserBackend::new(None)))
            .with_locator(Box::new(MemoryLocator::new()))
            .with_rtp_config(RtpConfig { comfort_noise: false, ..Default::default() })
            .with_network_profiles(profiles, "wan".into())
            .register_module("acl", AclModule::create)
            .register_module("call", CallModule::create)
            .build().await.unwrap();
        let serving = tokio::spawn(async move { server.serve().await });
        let mut children = Vec::new();
        for (role, port, expected) in [("uas", uas_port, b_ip), ("uac", uac_port, a_ip)] {
            let template = std::fs::read_to_string(root.join(format!("tests/sipp/media_network_{role}.xml"))).unwrap();
            let scenario = logs.join(format!("{role}.xml"));
            std::fs::write(&scenario, template.replace("@EXPECTED_IP@", &expected.replace('.', "[.]"))).unwrap();
            let mut command = tokio::process::Command::new(std::env::var("SIPP_BIN").unwrap_or_else(|_| "sipp".into()));
            if role == "uac" { command.arg(format!("127.0.0.1:{pbx_port}")); }
            let mut child = command.current_dir(&logs).arg("-sf").arg(&scenario)
                .args(["-i", "127.0.0.1", "-p", &port.to_string(), "-t", "u1", "-m", "1",
                    "-timeout", "20s", "-timeout_error", "-nostdin",
                    "-trace_msg", "-message_file", &format!("{role}-messages.log"),
                    "-trace_err", "-error_file", &format!("{role}-errors.log"),
                    "-trace_logs", "-log_file", &format!("{role}-actions.log")])
                .stdin(std::process::Stdio::null())
                .stdout(std::fs::File::create(logs.join(format!("{role}-stdout.log"))).unwrap())
                .stderr(std::fs::File::create(logs.join(format!("{role}-stderr.log"))).unwrap())
                .kill_on_drop(true).spawn().expect("start SIPp");
            if role == "uas" {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        assert!(child.try_wait().unwrap().is_none(), "SIPp startup failed: {}", logs.display());
                        match std::net::UdpSocket::bind(("127.0.0.1", port)) {
                            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => break,
                            Err(e) => panic!("check SIPp listener: {e}"),
                            Ok(socket) => drop(socket),
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await.unwrap();
            }
            children.push(child);
        }
        let statuses = tokio::time::timeout(Duration::from_secs(25), async {
            let (uas, uac) = children.split_at_mut(1);
            tokio::join!(uas[0].wait(), uac[0].wait())
        }).await;
        token.cancel();
        serving.abort();
        let (uas, uac) = statuses.expect("SIPp must finish before deadline");
        let (uas, uac) = (uas.unwrap(), uac.unwrap());
        assert!(uas.success() && uac.success(), "{case}: UAS={uas}, UAC={uac}; logs: {}", logs.display());
        let a_actions = std::fs::read_to_string(logs.join("uac-actions.log")).unwrap();
        let b_actions = std::fs::read_to_string(logs.join("uas-actions.log")).unwrap();
        assert!(a_actions.contains("A_183_NETWORK_OK") && a_actions.contains("A_200_NETWORK_OK"));
        assert!(b_actions.contains("B_INVITE_NETWORK_OK"));
        let a_messages = std::fs::read_to_string(logs.join("uac-messages.log")).unwrap();
        let ports: Vec<_> = regex::Regex::new(r"m=audio (2[0-4][0-9]{3}) RTP/AVP").unwrap()
            .captures_iter(&a_messages).map(|c| c[1].to_string()).collect();
        assert!(ports.len() >= 2 && ports.iter().all(|p| p == &ports[0]), "A-leg 183/200 must retain the same RTP socket");
        println!("{case}: PASS A={a_ip}:{} (183 + 200), B={b_ip} (INVITE), no caller RTP; {}", ports[0], logs.display());
    }
}
