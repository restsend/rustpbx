use crate::call::user::SipUser;
use crate::config::ProxyConfig;
use crate::proxy::server::SipServerBuilder;

pub fn test_proxy_config(port: u16) -> ProxyConfig {
    ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        tcp_port: None,
        tls_port: None,
        ws_port: None,
        useragent: Some("RustPBX-Test/0.1.0".to_string()),
        modules: Some(vec![
            "auth".to_string(),
            "registrar".to_string(),
            "call".to_string(),
        ]),
        ..Default::default()
    }
}

pub fn test_proxy_config_with_modules(port: u16, extra_modules: &[&str]) -> ProxyConfig {
    let mut modules = vec![
        "auth".to_string(),
        "registrar".to_string(),
        "call".to_string(),
    ];
    for m in extra_modules {
        modules.push((*m).to_string());
    }
    ProxyConfig {
        addr: "127.0.0.1".to_string(),
        udp_port: Some(port),
        tcp_port: None,
        tls_port: None,
        ws_port: None,
        useragent: Some("RustPBX-Test/0.1.0".to_string()),
        modules: Some(modules),
        ..Default::default()
    }
}

pub fn standard_test_users() -> Vec<SipUser> {
    vec![
        SipUser {
            id: 1,
            username: "alice".to_string(),
            password: Some("password123".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            is_support_webrtc: true,
            voicemail_disabled: true,
            ..Default::default()
        },
        SipUser {
            id: 2,
            username: "bob".to_string(),
            password: Some("password456".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            is_support_webrtc: false,
            voicemail_disabled: true,
            ..Default::default()
        },
        SipUser {
            id: 3,
            username: "charlie".to_string(),
            password: Some("password789".to_string()),
            enabled: true,
            realm: Some("127.0.0.1".to_string()),
            is_support_webrtc: true,
            voicemail_disabled: true,
            ..Default::default()
        },
    ]
}

pub fn register_standard_modules(builder: SipServerBuilder) -> SipServerBuilder {
    use crate::proxy::{auth::AuthModule, call::CallModule, registrar::RegistrarModule};

    builder
        .register_module("registrar", |inner, config| {
            Ok(Box::new(RegistrarModule::new(inner, config)))
        })
        .register_module("auth", |inner, _config| {
            Ok(Box::new(AuthModule::new(
                inner.clone(),
                inner.proxy_config.clone(),
            )))
        })
        .register_module("call", |inner, config| {
            Ok(Box::new(CallModule::new(config, inner)))
        })
}

pub fn build_sdp(ip: &str, port: u16, codecs: &[(u8, &str)]) -> String {
    let pt_list: Vec<String> = codecs.iter().map(|(pt, _)| pt.to_string()).collect();
    let rtpmap_lines: Vec<String> = codecs
        .iter()
        .map(|(pt, spec)| format!("a=rtpmap:{} {}\r\n", pt, spec))
        .collect();
    let session_id = chrono::Utc::now().timestamp();

    format!(
        "v=0\r\n\
         o=- {sid} {sid} IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP {pts}\r\n\
         {rtpmaps}\
         a=sendrecv\r\n",
        sid = session_id,
        ip = ip,
        port = port,
        pts = pt_list.join(" "),
        rtpmaps = rtpmap_lines.join(""),
    )
}

pub fn pcmu_sdp(ip: &str, port: u16) -> String {
    build_sdp(ip, port, &[(0, "PCMU/8000"), (101, "telephone-event/8000")])
}

pub fn pcma_sdp(ip: &str, port: u16) -> String {
    build_sdp(ip, port, &[(8, "PCMA/8000"), (101, "telephone-event/8000")])
}

pub fn make_sdp(port: u16) -> String {
    pcmu_sdp("127.0.0.1", port)
}
