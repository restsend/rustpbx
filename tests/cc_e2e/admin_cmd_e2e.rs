//! CC 软电话管理指令（收集日志/截图）e2e —— `/ami/v1/cc/admin_cmd` 挂载与投递链路。
//!
//! 回归门（2026-10-15）：该端点曾被 `commerce` cargo feature 双重门控
//! （`CcAddon::ami_routes` 与 `handler::ami::ami_router` 的 addon 合并点都在
//! `#[cfg(feature = "commerce")]` 内），而带截图/收集日志按钮的 console SPA
//! 随任意 `addon-cc` 构建发布 —— 纯 `contact-center` 构建每次点击都 404。
//!
//! 钉住的契约：
//!
//! 1. 端点经生产合并链路挂载：`CcAddon::ami_routes` →
//!    `AddonRegistry::get_ami_routes` → `ami_router`。`addon-cc`
//!    构建即挂载，与 commerce 无关；
//! 2. handler 错误是 JSON 形状（router 404 的 body 为空）——console 据此区分
//!    「坐席不在线」与「路由缺失」；
//! 3. 完整投递闭环可用：HTTP create → SIP MESSAGE pager 到坐席 → 坐席
//!    answer POST → create 长轮询原样带回载荷（日志包/截图 base64）。

use rustpbx::app::{AppState, AppStateBuilder};
use rustpbx::call::Location;
use rustpbx::config::{AmiConfig, Config, ConsoleConfig, ProxyConfig};
use rsipstack::sip::{HostWithPort, Transport};
use rsipstack::transport::SipAddr;
use std::time::Duration;
use tower::ServiceExt;

const AMI: &str = "/ami/v1";

/// 构建带 cc addon（经 `[proxy].addons` 启用）的 AppState。
///
/// `bind_sip = true` 时绑定真实 UDP 端口（完整闭环测试需要能向坐席
/// 发送 MESSAGE）；否则跳过绑定（挂载/校验类断言不需要传输面）。
async fn build_app(bind_sip: bool) -> AppState {
    build_app_with_ice(bind_sip, None).await
}

/// 同 [`build_app`]，但可注入 `[[ice_servers]]`（信封 ICE 注入断言用）。
async fn build_app_with_ice(
    bind_sip: bool,
    ice_servers: Option<Vec<rustpbx::config::IceServerConfig>>,
) -> AppState {
    let port = portpicker::pick_unused_port().unwrap_or(25060);
    let proxy = ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: if bind_sip { Some(port) } else { None },
        tcp_port: None,
        tls_port: None,
        ws_port: None,
        ensure_user: Some(false),
        // 生产合并链路按 `[proxy].addons` 判定 addon 是否启用。
        addons: Some(vec!["cc".to_string()]),
        ..Default::default()
    };
    let config = Config {
        database_url: "sqlite::memory:".to_string(),
        proxy,
        ami: Some(AmiConfig::default()),
        console: Some(ConsoleConfig::default()),
        ice_servers,
        ..Default::default()
    };
    let mut builder = AppStateBuilder::new().with_config(config);
    if !bind_sip {
        builder = builder.with_skip_sip_bind();
    }
    builder.build().await.expect("build app state")
}

/// 与 `app.rs` 相同的 AMI 路由组装方式（含 addon 合并点）。
fn ami_router(app: &AppState) -> axum::Router {
    rustpbx::handler::ami_router(app.clone()).with_state(app.clone())
}

async fn post_json(
    router: &axum::Router,
    path: &str,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                // oneshot 没有 ConnectInfo：借 X-Forwarded-For 声明本机来源，
                // 命中默认 AMI localhost 允许名单。
                .header("x-forwarded-for", "127.0.0.1")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// 契约 1+2：路由在 addon-cc 构建挂载，且校验/未注册错误都是 handler JSON
/// 形状 —— router 级 404 的 body 为空，console 拿到的任何 404 都必须可辨。
#[tokio::test]
async fn admin_cmd_route_mounted_and_errors_are_handler_shaped() {
    let app = build_app(false).await;
    let router = ami_router(&app);

    // 缺 agent → 400 + handler JSON。
    let (status, body) = post_json(
        &router,
        &format!("{AMI}/cc/admin_cmd"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["message"], "missing agent", "handler must own the error");

    // 缺 cmd → 400。
    let (status, body) = post_json(
        &router,
        &format!("{AMI}/cc/admin_cmd"),
        serde_json::json!({"agent": "ghost"}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body["message"], "missing cmd");

    // 未注册坐席 → handler 形状的 404（JSON body 非空，可区别于路由缺失）。
    let (status, body) = post_json(
        &router,
        &format!("{AMI}/cc/admin_cmd"),
        serde_json::json!({"agent": "ghost", "cmd": "shot"}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "body: {body}");
    assert_eq!(body["message"], "agent not registered");

    // answer 端点同样挂载：未知 trans_id 静默 200（迟到/重复回执语义）。
    let (status, body) = post_json(
        &router,
        &format!("{AMI}/cc/admin_cmd_answer"),
        serde_json::json!({"trans_id": "no-such-trans", "accepted": false}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");
    assert_eq!(body["status"], "ok");
}

/// 契约 3：完整闭环 —— create 长轮询 → SIP MESSAGE 投给坐席 → 坐席 answer
/// POST → create 原样带回载荷。坐席用本机 UDP socket 模拟（rustpbx 只透传
/// 信封，不解析载荷）。
#[tokio::test]
async fn admin_cmd_full_loop_answer_round_trip() {
    let app = build_app(true).await;
    let router = ami_router(&app);

    // 假坐席：本机 UDP socket 充当软电话传输地址。
    let agent_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let agent_addr = agent_sock.local_addr().unwrap();
    let destination = SipAddr {
        r#type: Some(Transport::Udp),
        addr: HostWithPort::try_from(agent_addr.to_string()).unwrap(),
    };
    app.sip_server
        .inner
        .locator
        .register(
            "1001",
            Some("localhost"),
            Location {
                aor: "sip:1001@localhost".try_into().unwrap(),
                registered_aor: Some("sip:1001@localhost".try_into().unwrap()),
                expires: 120,
                destination: Some(destination),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // create 长轮询放后台（answer 到达前不返回）。
    let create = tokio::spawn({
        let router = router.clone();
        async move {
            post_json(
                &router,
                &format!("{AMI}/cc/admin_cmd"),
                serde_json::json!({"agent": "1001", "cmd": "log", "days": 1}),
            )
            .await
        }
    });

    // 坐席收到 MESSAGE：信封原样透传（cmd/days 保留），提取注入的 trans_id。
    let mut buf = vec![0u8; 65535];
    let (len, _) = tokio::time::timeout(Duration::from_secs(5), agent_sock.recv_from(&mut buf))
        .await
        .expect("agent must receive the admin-cmd MESSAGE within 5s")
        .unwrap();
    let raw = String::from_utf8_lossy(&buf[..len]).to_string();
    assert!(
        raw.contains("\"cmd\":\"log\"") && raw.contains("\"days\":1"),
        "envelope must be relayed verbatim: {raw}"
    );
    let marker = "\"trans_id\":\"";
    let idx = raw
        .find(marker)
        .expect("envelope must carry an injected trans_id")
        + marker.len();
    let trans_id = raw[idx..].split('"').next().unwrap().to_string();

    // 坐席应答（模拟日志包/截图 base64）。
    let payload = "aGVsbG8tcG5n"; // base64("hello-png")
    let (status, body) = post_json(
        &router,
        &format!("{AMI}/cc/admin_cmd_answer"),
        serde_json::json!({"trans_id": trans_id, "accepted": true, "answer": payload}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");
    assert_eq!(body["status"], "ok");

    // create 长轮询带回载荷。
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), create)
        .await
        .expect("create long-poll must resolve once the answer arrives")
        .unwrap();
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");
    assert_eq!(body["accepted"], true);
    assert_eq!(body["answer"], payload);
    assert_eq!(body["trans_id"], trans_id);

    app.token().cancel();
}

/// ICE servers 注入：配置 `[[ice_servers]]` 后，投给坐席的信封必须携带
/// `ice_servers`（console 浏览器 offerer 与坐席 answerer 用同一份部署
/// STUN/TURN，含 TURN REST 短时凭据）；未配置则不注入（旧客户端 serde
/// default 忽略，零回归）。
#[tokio::test]
async fn admin_cmd_envelope_carries_server_ice_servers() {
    let ice: rustpbx::config::IceServerConfig =
        serde_json::from_value(serde_json::json!({"urls": ["stun:stun.example:3478"]}))
            .expect("ice server config");
    let app = build_app_with_ice(true, Some(vec![ice])).await;
    let router = ami_router(&app);

    let agent_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let agent_addr = agent_sock.local_addr().unwrap();
    app.sip_server
        .inner
        .locator
        .register(
            "1001",
            Some("localhost"),
            Location {
                aor: "sip:1001@localhost".try_into().unwrap(),
                registered_aor: Some("sip:1001@localhost".try_into().unwrap()),
                expires: 120,
                destination: Some(SipAddr {
                    r#type: Some(Transport::Udp),
                    addr: HostWithPort::try_from(agent_addr.to_string()).unwrap(),
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let create = tokio::spawn({
        let router = router.clone();
        async move {
            post_json(
                &router,
                &format!("{AMI}/cc/admin_cmd"),
                serde_json::json!({"agent": "1001", "cmd": "shot", "target": "mini"}),
            )
            .await
        }
    });

    let mut buf = vec![0u8; 65535];
    let (len, _) = tokio::time::timeout(Duration::from_secs(5), agent_sock.recv_from(&mut buf))
        .await
        .expect("agent must receive the admin-cmd MESSAGE")
        .unwrap();
    let raw = String::from_utf8_lossy(&buf[..len]).to_string();
    let body = raw
        .split("\r\n\r\n")
        .nth(1)
        .expect("MESSAGE must carry a body");
    let envelope: serde_json::Value = serde_json::from_str(body).expect("envelope JSON");
    let injected = envelope["ice_servers"]
        .as_array()
        .expect("envelope must carry injected ice_servers");
    assert_eq!(injected.len(), 1, "one configured STUN entry");
    assert_eq!(injected[0]["urls"][0], "stun:stun.example:3478");

    // 回执闭合长轮询（避免 20s 超时占用）。
    let trans_id = envelope["trans_id"].as_str().unwrap().to_string();
    let (status, _) = post_json(
        &router,
        &format!("{AMI}/cc/admin_cmd_answer"),
        serde_json::json!({"trans_id": trans_id, "accepted": true, "answer": "v=0 ok"}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let (status, body) = tokio::time::timeout(Duration::from_secs(5), create)
        .await
        .expect("create long-poll must resolve once the answer arrives")
        .unwrap();
    assert_eq!(status, axum::http::StatusCode::OK, "body: {body}");
    assert_eq!(body["accepted"], true);

    app.token().cancel();
}
