//! Opt-in real service test: SIP + PCMU RTP -> PBX -> quality service -> BYE/CDR/WAV/SIPFlow.
use std::{sync::Arc, time::Duration};
use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use std::io::Write;
use crate::common::{test_helpers, test_ua::{TestUa, TestUaConfig, TestUaEvent}, rtp_utils::RtpPacket};
use rustpbx::{call::quality::{QualityConfig, QualitySides}, config::{MediaProxyMode, RecordingPolicy, SipFlowConfig}};

#[tokio::test]
#[ignore = "requires QUALITY_E2E_URL, QUALITY_E2E_AUDIO_DIR and QUALITY_E2E_OUTPUT; real service/models"]
async fn voicemail_service_hangs_up_real_calls_and_finalizes_artifacts() -> Result<()> {
    let _ = tracing_subscriber::fmt().with_env_filter("rustpbx::proxy::proxy_call::sip_session=debug").with_test_writer().try_init();
    let url = std::env::var("QUALITY_E2E_URL")?;
    let inputs = std::path::PathBuf::from(std::env::var("QUALITY_E2E_AUDIO_DIR")?);
    let root = std::path::PathBuf::from(std::env::var("QUALITY_E2E_OUTPUT")?);
    for (case, language, method) in [("en", "en", "asr"), ("id", "id", "asr"), ("beep", "en", "beep")] {
        let output = root.join(case);
        std::fs::create_dir_all(&output)?;
        // Transparent recorder: forwards every message unchanged to the real service.
        let relay_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let relay_url = format!("ws://{}", relay_listener.local_addr()?);
        let upstream_url = url.clone();
        let capture_dir = output.clone();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let relay_task = tokio::spawn(async move {
            let (socket, _) = relay_listener.accept().await?;
            let mut pbx = tokio_tungstenite::accept_async(socket).await?;
            let (mut upstream, _) = tokio_tungstenite::connect_async(upstream_url).await?;
            let mut pcm = std::fs::File::create(capture_dir.join("quality-input.pcm"))?;
            let mut events = std::fs::File::create(capture_dir.join("quality-events.jsonl"))?;
            loop {
                tokio::select! {
                    message = pbx.next() => match message {
                        Some(Ok(message)) => {
                            if message.is_text() { std::fs::write(capture_dir.join("quality-start.json"), message.to_text()?)?; }
                            if message.is_binary() { pcm.write_all(&message.clone().into_data())?; }
                            let close = message.is_close();
                            upstream.send(message).await?;
                            if close { let _ = pbx.flush().await; break; }
                        },
                        _ => break,
                    },
                    message = upstream.next() => match message {
                        Some(Ok(message)) => {
                            if message.is_text() { writeln!(events, "{}", message.to_text()?)?; }
                            let close = message.is_close();
                            pbx.send(message).await?;
                            if close { let _ = upstream.flush().await; break; }
                        },
                        _ => break,
                    },
                }
            }
            let _ = closed_tx.send(());
            Ok::<_, anyhow::Error>(())
        });
        let port = portpicker::pick_unused_port().context("SIP port")?;
        let mut config = test_helpers::test_proxy_config(port);
        config.media_proxy = MediaProxyMode::All;
        config.ensure_user = Some(false);
        config.enable_latching = false;
        config.recording = Some(RecordingPolicy { enabled: Some(true), auto_start: Some(true), path: Some(output.join("recordings").to_string_lossy().into()), ..Default::default() });
        config.quality = Some(QualityConfig { url: relay_url, enabled: true, sides: QualitySides::Callee,
            parameters: serde_json::json!({"asr_enabled":method == "asr", "beep_enabled":method == "beep", "asr_languages":["id", "en"],
                "text_language":language, "hangup_on_asr":true, "hangup_on_beep":true, "inspect_ms":15000}).as_object().unwrap().clone(),
            ..Default::default() });
        let cancel = tokio_util::sync::CancellationToken::new();
        let _cleanup = cancel.clone().drop_guard();
        let flow_config: SipFlowConfig = serde_json::from_value(serde_json::json!({"type":"local", "root":output.join("sipflow"), "flush_count":1, "flush_interval_secs":1}))?;
        let flow = rustpbx::sipflow::backend::create_backend(&flow_config, cancel.clone()).await?;
        let users = rustpbx::proxy::user::MemoryUserBackend::new(None);
        for user in test_helpers::standard_test_users() { users.create_user(user).await?; }
        let (record_tx, mut records) = tokio::sync::mpsc::channel(8);
        let builder = rustpbx::proxy::server::SipServerBuilder::new(Arc::new(config))
            .with_user_backend(Box::new(users)).with_locator(Box::new(rustpbx::proxy::locator::MemoryLocator::new()))
            .with_cancel_token(cancel.clone()).with_sipflow_backend(Some(flow.clone())).with_callrecord_sender(Some(record_tx));
        let server = Arc::new(test_helpers::register_standard_modules(builder).build().await?);
        let registry = server.get_inner().active_call_registry.clone();
        let server_task = tokio::spawn(async move { server.serve().await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut uas = Vec::new();
        for (name, password) in [("alice", "password123"), ("bob", "password456")] {
            let mut ua = TestUa::new(TestUaConfig { webrtc:false, username:name.into(), password:password.into(), realm:"127.0.0.1".into(),
                local_port:portpicker::pick_unused_port().unwrap(), proxy_addr:format!("127.0.0.1:{port}").parse()? });
            ua.start().await?;
            ua.register().await?;
            uas.push(Arc::new(ua));
        }
        let alice = uas[0].clone();
        let bob = uas[1].clone();
        let caller_rtp = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let callee_rtp = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let caller_sdp = test_helpers::pcmu_sdp("127.0.0.1", caller_rtp.local_addr()?.port());
        let callee_sdp = test_helpers::pcmu_sdp("127.0.0.1", callee_rtp.local_addr()?.port());
        let caller = alice.clone();
        let calling = tokio::spawn(async move { caller.make_call("bob", Some(caller_sdp)).await });
        let (callee_dialog, callee_target) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for event in bob.process_dialog_events().await? {
                    if let TestUaEvent::IncomingCall(id, Some(offer)) = event {
                        let address = rustpbx::sipflow::extract_rtp_addr(&offer).context("callee RTP offer address")?;
                        bob.answer_call(&id, Some(callee_sdp.clone())).await?;
                        return Ok::<_, anyhow::Error>((id, address));
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await??;
        let caller_dialog = tokio::time::timeout(Duration::from_secs(5), calling).await???;
        let answer = alice.get_negotiated_answer_sdp(&caller_dialog).await.context("caller answer SDP")?;
        let caller_target = rustpbx::sipflow::extract_rtp_addr(&answer).context("caller RTP answer address")?;
        let samples = if case == "beep" {
            // Half a second of silence then a configured 1309 Hz voicemail tone.
            (0..8000).map(|i| if i < 4000 { 0 } else { (12000.0 * (std::f64::consts::TAU * 1309.0 * (i - 4000) as f64 / 8000.0).sin()) as i16 }).collect::<Vec<_>>()
        } else {
            let mut wav = rustpbx::media::wav_reader::WavReader::open(inputs.join(format!("voicemail-{case}.wav")))?;
            anyhow::ensure!(wav.spec().sample_rate == 8000 && wav.spec().channels == 1 && wav.spec().bits_per_sample == 16, "8 kHz mono PCM16 fixture required");
            wav.samples().collect::<std::result::Result<Vec<_>, _>>()?
        };
        let media_cancel = cancel.child_token();
        let _media_cleanup = media_cancel.clone().drop_guard();
        let stop = media_cancel.clone();
        let sender = tokio::spawn(async move {
            let mut encoder = audio_codec::create_encoder(audio_codec::CodecType::PCMU);
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            for index in 0..750u32 {
                tokio::select! { _ = stop.cancelled() => break, _ = tick.tick() => {} }
                let begin = index as usize * 160;
                let mut frame = [0i16; 160];
                if begin < samples.len() {
                    let count = 160.min(samples.len() - begin);
                    frame[..count].copy_from_slice(&samples[begin..begin+count]);
                }
                let packet = RtpPacket::new(0, index as u16, index*160, 0x22334455, encoder.encode(&frame)).encode();
                callee_rtp.send_to(&packet, &callee_target).await?;
                let silence = RtpPacket::new(0, index as u16, index*160, 0x11223344, vec![0xff; 160]).encode();
                caller_rtp.send_to(&silence, &caller_target).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let record = tokio::time::timeout(Duration::from_secs(18), records.recv()).await.context("quality service did not hang up within 18 seconds")?.context("terminal CDR")?;
        media_cancel.cancel();
        sender.await??;
        std::fs::write(output.join("cdr.json"), serde_json::to_vec_pretty(&record)?)?;
        assert_eq!(record.status_code, 200, "{case}: answered call status");
        assert!(record.answer_time.is_some());
        assert!(record.end_time > record.answer_time.unwrap());
        assert!(matches!(record.hangup_reason, Some(rustpbx::callrecord::CallRecordHangupReason::Autohangup)));
        let trace = record.details.metadata.as_ref().unwrap()["trace"].as_array().unwrap();
        let observation = trace.iter().find(|e| e["code"] == "quality.voicemail_detected").context("actual service detection trace")?;
        assert_eq!(observation["side"], "callee");
        assert_eq!(observation["detail"]["method"], method);
        if method == "asr" { assert_eq!(observation["detail"]["language"], language); }
        let ends: Vec<_> = trace.iter().filter(|e| e["kind"] == "end").collect();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["detail"]["source"], "realtime_quality");
        assert_eq!(ends[0]["detail"]["reason"], "quality.voicemail_detected");
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut a, mut b) = (false, false);
            loop {
                a |= alice.process_dialog_events().await?.iter().any(|e| matches!(e, TestUaEvent::CallTerminated(id) if id == &caller_dialog));
                b |= bob.process_dialog_events().await?.iter().any(|e| matches!(e, TestUaEvent::CallTerminated(id) if id == &callee_dialog));
                if a && b && registry.get_handle(&record.call_id).is_none() && registry.handles_by_dialog_count() == 0 && registry.dialog_by_session_count() == 0 { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        }).await.context("BYE and registry cleanup")??;
        tokio::time::timeout(Duration::from_secs(3), closed_rx).await.context("quality WebSocket teardown")??;
        relay_task.await??;
        assert_eq!(record.recorder.len(), 1, "local recording CDR entry");
        let mut wav = rustpbx::media::wav_reader::WavReader::open(&record.recorder[0].path)?;
        let wave: Vec<i16> = wav.samples().collect::<std::result::Result<_,_>>()?;
        assert!(!wave.is_empty() && wave.iter().any(|s| s.unsigned_abs() > 1000), "recording must contain real non-silent audio");
        let size = std::fs::metadata(&record.recorder[0].path)?.len();
        flow.flush().await?;
        let from = (record.start_time - chrono::Duration::seconds(2)).with_timezone(&chrono::Local);
        let to = (record.end_time + chrono::Duration::seconds(2)).with_timezone(&chrono::Local);
        let signaling = flow.query_flow(&record.call_id, from, to).await?;
        std::fs::write(output.join("sipflow.json"), serde_json::to_vec_pretty(&signaling)?)?;
        let text = signaling.iter().map(|p| String::from_utf8_lossy(&p.payload)).collect::<Vec<_>>().join("\n");
        for method in ["INVITE ", "ACK ", "BYE ", "SIP/2.0 200"] { assert!(text.contains(method), "{case}: missing {method} in SIPFlow"); }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(std::fs::metadata(&record.recorder[0].path)?.len(), size, "recording stopped growing");
        assert!(records.try_recv().is_err(), "exactly one terminal CDR");
        eprintln!("QUALITY_E2E_OK case={case} call_id={} duration_ms={} recording={} bytes={size} sipflow_items={} registry_handles={}", record.call_id,
            (record.end_time - record.answer_time.unwrap()).num_milliseconds(), record.recorder[0].path, signaling.len(), registry.handles_by_dialog_count());
        for ua in uas { ua.stop(); }
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(3), server_task).await???;
    }
    Ok(())
}
