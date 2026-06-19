use crate::config::{MediaProxyMode, ProxyConfig};
use crate::proxy::routing::TrunkConfig;
use crate::proxy::tests::e2e_test_server::E2eTestServer;
use anyhow::Result;
use rsipstack::EndpointBuilder;
use rsipstack::sip::{
    Method, Param, SipMessage, Version,
    headers::CallId,
    headers::typed::CSeq,
    typed::{From as FromHeader, To as ToHeader, Via},
    uri::Tag,
};
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::transaction::Transaction;
use rsipstack::transport::TransportLayer;
use rsipstack::transport::udp::UdpConnection;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::info;

fn trunk_options_proxy_config() -> ProxyConfig {
    let mut trunks = HashMap::new();
    trunks.insert(
        "carrier_a".to_string(),
        TrunkConfig {
            dest: "sip:10.68.144.74:5060".to_string(),
            inbound_hosts: vec!["10.68.144.74".to_string()],
            ..Default::default()
        },
    );
    ProxyConfig {
        media_proxy: MediaProxyMode::All,
        trunks,
        ..Default::default()
    }
}

async fn send_options(server_addr: &str, local_port: u16) -> Result<Option<u16>> {
    let cancel = CancellationToken::new();
    let tl = TransportLayer::new(cancel.child_token());
    let udp = UdpConnection::create_connection(
        format!("127.0.0.1:{local_port}").parse()?,
        None,
        Some(cancel.child_token()),
    )
    .await?;
    tl.add_transport(udp.into());

    let mut builder = EndpointBuilder::new();
    builder.with_user_agent("test-options-client/1.0");
    builder.with_transport_layer(tl);
    builder.with_cancel_token(cancel.child_token());
    builder.with_timer_interval(Duration::from_millis(50));
    let endpoint = builder.build();

    let ep_inner = endpoint.inner.clone();
    let ct = cancel.clone();
    crate::utils::spawn(async move {
        tokio::select! {
            _ = ct.cancelled() => {}
            r = ep_inner.serve() => { if let Err(e) = r { tracing::warn!("client serve: {e}"); } }
        }
    });

    sleep(Duration::from_millis(100)).await;

    let local_addr = format!("127.0.0.1:{local_port}");
    let branch = format!(
        "z9hG4bK-test{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    );
    let tag = uuid::Uuid::new_v4()
        .to_string()
        .split('-')
        .next()
        .unwrap_or("tag")
        .to_string();

    let via = Via::parse(&format!("SIP/2.0/UDP {local_addr};branch={branch}"))?;
    let from = FromHeader {
        display_name: None,
        uri: format!("sip:health-check@{local_addr}").parse()?,
        params: vec![Param::Tag(Tag::new(&tag))],
    };
    let to = ToHeader {
        display_name: None,
        uri: format!("sip:{server_addr}").parse()?,
        params: vec![],
    };
    let call_id = CallId::new(format!(
        "test-{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));

    let request = rsipstack::sip::Request {
        method: Method::Options,
        uri: format!("sip:{server_addr}").parse()?,
        version: Version::V2,
        headers: vec![
            via.into(),
            from.into(),
            to.into(),
            call_id.into(),
            CSeq {
                seq: 1,
                method: Method::Options,
            }
            .into(),
        ]
        .into(),
        body: vec![],
    };

    let key = TransactionKey::from_request(&request, TransactionRole::Client)?;
    let mut tx = Transaction::new_client(key, request, endpoint.inner.clone(), None);
    tx.send().await?;

    let result = tokio::time::timeout(Duration::from_secs(3), tx.receive()).await;
    cancel.cancel();

    match result {
        Ok(Some(SipMessage::Response(resp))) => Ok(Some(u16::from(resp.status_code))),
        Ok(Some(_)) => Ok(None),
        Ok(None) => Ok(None),
        Err(_) => Ok(None),
    }
}

#[tokio::test]
async fn test_options_from_trunk_ip_gets_200_ok() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let mut config = trunk_options_proxy_config();
    config.trunks.insert(
        "local_test".to_string(),
        TrunkConfig {
            dest: "sip:carrier.example.com:5060".to_string(),
            inbound_hosts: vec!["127.0.0.1".to_string()],
            ..Default::default()
        },
    );

    let server = E2eTestServer::start_with_config(config).await?;
    sleep(Duration::from_millis(200)).await;

    let client_port = portpicker::pick_unused_port().unwrap();
    let server_addr = format!("127.0.0.1:{}", server.port);
    let status = send_options(&server_addr, client_port).await?;

    assert_eq!(status, Some(200), "OPTIONS from trunk IP should get 200 OK");

    server.stop();
    info!("test_options_from_trunk_ip_gets_200_ok PASSED");
    Ok(())
}

#[tokio::test]
async fn test_options_from_unknown_ip_gets_no_response() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    let mut config = trunk_options_proxy_config();
    config.trunks.insert(
        "carrier_only".to_string(),
        TrunkConfig {
            dest: "sip:10.68.144.74:5060".to_string(),
            inbound_hosts: vec!["10.68.144.74".to_string()],
            ..Default::default()
        },
    );

    let server = E2eTestServer::start_with_config(config).await?;
    sleep(Duration::from_millis(200)).await;

    let client_port = portpicker::pick_unused_port().unwrap();
    let server_addr = format!("127.0.0.1:{}", server.port);
    let status = send_options(&server_addr, client_port).await?;

    assert_eq!(
        status, None,
        "OPTIONS from non-trunk IP should get no response (dropped)"
    );

    server.stop();
    info!("test_options_from_unknown_ip_gets_no_response PASSED");
    Ok(())
}
