use crate::app::AppState;
use crate::config::SipFlowConfig;
use crate::sipflow::protocol::{MsgType, Packet};
use crate::sipflow::storage::{StorageManager, process_packet};
use anyhow::Result;
use chrono::{DateTime, Local};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use std::net::IpAddr;
use std::path::PathBuf;

pub async fn run_fixtures(state: AppState) -> Result<()> {
    println!("Initializing fixtures...");
    let db = state.db();

    // 1. Initialize Extensions and Routes
    init_core_fixtures(db).await?;

    // 2. Initialize Demo User for demo mode
    if state.config().demo_mode {
        crate::models::user::Model::upsert_super_user(
            db,
            "demo@miuda.ai",
            "demo@miuda.ai",
            "hello@miuda.ai",
        )
        .await?;
        println!("Created demo superuser: demo@miuda.ai");
    }

    // 3. Initialize Addon Fixtures (calling each addon's seed_fixtures)
    state
        .addon_registry
        .seed_all_fixtures(state.clone())
        .await?;

    Ok(())
}

async fn init_core_fixtures(db: &sea_orm::DatabaseConnection) -> Result<()> {
    use crate::models::extension;
    use crate::models::routing;

    // Extensions
    for ext_num in ["1001", "1002"] {
        if extension::Entity::find()
            .filter(extension::Column::Extension.eq(ext_num))
            .one(db)
            .await?
            .is_none()
        {
            let ext = extension::ActiveModel {
                extension: Set(ext_num.to_string()),
                display_name: Set(Some(format!("Extension {}", ext_num))),
                sip_password: Set(Some("1234".to_string())),
                ..Default::default()
            };
            ext.insert(db).await?;
            println!("Created extension {}", ext_num);
        }
    }

    // Routing
    if routing::Entity::find()
        .filter(routing::Column::Name.eq("Default Outbound"))
        .one(db)
        .await?
        .is_none()
    {
        let route = routing::ActiveModel {
            name: Set("Default Outbound".to_string()),
            direction: Set(routing::RoutingDirection::Outbound),
            destination_pattern: Set(Some("^\\d+$".to_string())),
            ..Default::default()
        };
        route.insert(db).await?;
        println!("Created default outbound route");
    }

    Ok(())
}

pub async fn init_sipflow_demo_data(
    state: AppState,
    call_id: &str,
    start_time: DateTime<Local>,
) -> Result<()> {
    let sipflow_config = match &state.config().sipflow {
        Some(SipFlowConfig::Local {
            root,
            subdirs,
            flush_count,
            flush_interval_secs,
            id_cache_size,
            ..
        }) => (
            root,
            subdirs,
            flush_count,
            flush_interval_secs,
            id_cache_size,
        ),
        _ => return Ok(()),
    };

    let (root, subdirs, flush_count, flush_interval_secs, id_cache_size) = sipflow_config;
    let mut storage = StorageManager::new(
        &PathBuf::from(root),
        *flush_count,
        *flush_interval_secs,
        *id_cache_size,
        subdirs.clone(),
    );

    let base_ts = start_time.timestamp_micros() as u64;
    let src_ip: IpAddr = "127.0.0.1".parse().unwrap();
    let dst_ip: IpAddr = "127.0.0.1".parse().unwrap();

    let messages = vec![
        (0, "INVITE", 5060, 5060),
        (100, "100 Trying", 5060, 5060),
        (500, "180 Ringing", 5060, 5060),
        (1000, "200 OK", 5060, 5060),
        (1100, "ACK", 5060, 5060),
        (60000, "BYE", 5060, 5060),
        (60100, "200 OK", 5060, 5060),
    ];

    for (offset_ms, content, src_p, dst_p) in messages {
        let payload = if content.contains("OK")
            || content.contains("Trying")
            || content.contains("Ringing")
        {
            format!(
                "SIP/2.0 {}\r\nFrom: <sip:1001@example.com>\r\nTo: <sip:12345678@example.com>\r\nCall-ID: {}\r\nContent-Length: 0\r\n\r\n",
                content, call_id
            )
        } else {
            format!(
                "{} sip:12345678@example.com SIP/2.0\r\nFrom: <sip:1001@example.com>\r\nTo: <sip:12345678@example.com>\r\nCall-ID: {}\r\nContent-Length: 0\r\n\r\n",
                content, call_id
            )
        };

        let packet = Packet {
            msg_type: MsgType::Sip,
            src: (src_ip, src_p),
            dst: (dst_ip, dst_p),
            timestamp: base_ts + (offset_ms * 1000),
            payload: payload.into(),
        };
        let processed = process_packet(packet);
        storage.write_processed(processed).await?;
    }

    // Add some RTP demo data
    // RTP Header: Version=2, P=0, X=0, CC=0, M=0, PT=0 (PCMU), Seq=1, TS=160, SSRC=123
    let mut rtp_payload = vec![0u8; 172]; // 12 bytes header + 160 bytes payload
    rtp_payload[0] = 0x80;
    rtp_payload[1] = 0x00;

    for i in 0..50 {
        let packet = Packet {
            msg_type: MsgType::Rtp,
            src: (src_ip, 10000),
            dst: (dst_ip, 20000),
            timestamp: base_ts + 2000 * 1000 + (i * 20 * 1000), // starting at 2s, 20ms apart
            payload: rtp_payload.clone().into(),
        };
        let mut processed = process_packet(packet);
        processed.callid = Some(call_id.to_string());
        processed.leg = Some(0); // Leg A
        processed.src = "127.0.0.1:10000".to_string();
        storage.write_processed(processed).await?;

        // Also Leg B
        let mut processed_b = process_packet(Packet {
            msg_type: MsgType::Rtp,
            src: (src_ip, 20000),
            dst: (dst_ip, 10000),
            timestamp: base_ts + 2000 * 1000 + (i * 20 * 1000) + 5000, // slight jitter
            payload: rtp_payload.clone().into(),
        });
        processed_b.callid = Some(call_id.to_string());
        processed_b.leg = Some(1); // Leg B
        processed_b.src = "127.0.0.1:20000".to_string();
        storage.write_processed(processed_b).await?;
    }

    storage.check_flush().await?;
    Ok(())
}
