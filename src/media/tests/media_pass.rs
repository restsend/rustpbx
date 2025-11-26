use crate::event::SessionEvent;
use crate::media::AudioFrame;
use crate::media::Samples;
use crate::media::codecs::bytes_to_samples;
use crate::media::track::Track;
use crate::media::track::media_pass::MediaPassOption;
use crate::media::track::media_pass::MediaPassTrack;
use crate::media::{PcmBuf, Sample};
use anyhow::Result;
use bytes::Bytes;
use futures::FutureExt;
use futures::StreamExt;
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use warp;
use warp::Filter;
use warp::ws::Ws;

// generate 1 second audio samples
fn generate_audio_samples(sample_rate: u32) -> PcmBuf {
    let num_samples = sample_rate * 1;
    let mut samples = vec![0; num_samples as usize];
    let frequency = 440.0;
    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let amplitude = 16384.0; // Half of 16-bit range for safety (32768 / 2)
        let sample = (amplitude * (2.0 * std::f32::consts::PI * frequency * t).sin()) as Sample;
        samples[i as usize] = sample;
    }
    samples
}

#[derive(Deserialize, Debug)]
struct Params {
    sample_rate: Option<u32>,
    packet_size: Option<u32>,
}

async fn start_mock_server(
    cancel_token: CancellationToken,
    sample_rate_holder: Arc<AtomicU32>,
    packet_size_holder: Arc<AtomicU32>,
    received_data: Option<Arc<tokio::sync::Mutex<Vec<u8>>>>,
    received_timestamps: Option<Arc<tokio::sync::Mutex<Vec<Instant>>>>,
) -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let routes = warp::any()
        .and(warp::ws())
        .and(warp::query::<Params>())
        .map(move |ws: Ws, params: Params| {
            if let Some(sample_rate) = params.sample_rate {
                sample_rate_holder.store(sample_rate, Ordering::Relaxed);
            }
            if let Some(packet_size) = params.packet_size {
                packet_size_holder.store(packet_size, Ordering::Relaxed);
            }

            let received_data = received_data.clone();
            let received_timestamps = received_timestamps.clone();

            ws.on_upgrade(move |websocket| {
                let (tx, rx) = websocket.split();
                let received_data = received_data.clone();
                let received_timestamps = received_timestamps.clone();

                async move {
                    if received_data.is_some() && received_timestamps.is_some() {
                        // Capture mode: capture binary messages
                        let received_data = received_data.unwrap();
                        let received_timestamps = received_timestamps.unwrap();
                        let mut rx = rx;
                        while let Some(msg) = rx.next().await {
                            match msg {
                                Ok(msg) => {
                                    // Check if message is binary
                                    if msg.is_binary() {
                                        let bytes = msg.as_bytes();
                                        if !bytes.is_empty() {
                                            let mut data_guard = received_data.lock().await;
                                            data_guard.extend_from_slice(bytes);
                                            let mut timestamps_guard =
                                                received_timestamps.lock().await;
                                            timestamps_guard.push(Instant::now());
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    } else {
                        // Echo mode: just forward messages
                        rx.forward(tx)
                            .map(|result| {
                                if let Err(e) = result {
                                    warn!("websocket error: {:?}", e);
                                }
                            })
                            .await;
                    }
                }
            })
        });

    let server = warp::serve(routes).incoming(listener);
    tokio::spawn(async move {
        server
            .graceful(async move {
                cancel_token.cancelled().await;
            })
            .run()
            .await;
    });
    Ok(addr)
}

fn create_track(
    track_id: String,
    url: String,
    sending_sample_rate: u32,
    receiving_sample_rate: u32,
    packet_size: u32,
    cancel_token: CancellationToken,
    ptime: Option<u32>,
) -> Result<MediaPassTrack> {
    let option = MediaPassOption::new(
        url,
        sending_sample_rate,
        receiving_sample_rate,
        Some(packet_size),
        ptime,
    );
    let track = MediaPassTrack::new(
        "test_session_id".to_string(),
        0,
        track_id,
        cancel_token,
        option,
    );
    Ok(track)
}

// Mock server that sends audio data to the track (for ptime testing)
async fn start_sending_server(
    cancel_token: CancellationToken,
    sample_rate_holder: Arc<AtomicU32>,
    packet_size_holder: Arc<AtomicU32>,
    audio_data: Vec<u8>,
) -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let routes = warp::any()
        .and(warp::ws())
        .and(warp::query::<Params>())
        .map(move |ws: Ws, params: Params| {
            if let Some(sample_rate) = params.sample_rate {
                sample_rate_holder.store(sample_rate, Ordering::Relaxed);
            }
            if let Some(packet_size) = params.packet_size {
                packet_size_holder.store(packet_size, Ordering::Relaxed);
            }

            let audio_data = audio_data.clone();

            ws.on_upgrade(move |websocket| {
                let (mut tx, mut _rx) = websocket.split();
                let audio_data = audio_data.clone();
                async move {
                    use futures::SinkExt;
                    // Send audio data in small chunks
                    let chunk_size = 320; // 20ms at 16kHz = 320 samples = 640 bytes
                    let chunks: Vec<Vec<u8>> =
                        audio_data.chunks(chunk_size).map(|c| c.to_vec()).collect();
                    for chunk in chunks {
                        if let Err(_) = tx.send(warp::ws::Message::binary(Bytes::from(chunk))).await
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            })
        });

    let server = warp::serve(routes).incoming(listener);
    tokio::spawn(async move {
        server
            .graceful(async move {
                cancel_token.cancelled().await;
            })
            .run()
            .await;
    });
    Ok(addr)
}

#[tokio::test]
async fn test_media_pass() -> Result<()> {
    let track_id = "media_pass_track".to_string();
    let sending_sample_rate = 8000;
    let receiving_sample_rate = 16000;
    let packet_size = 640;
    let sample = generate_audio_samples(sending_sample_rate);
    let cancel_token = CancellationToken::new();
    let sample_rate_holder = Arc::new(AtomicU32::new(0));
    let packet_size_holder = Arc::new(AtomicU32::new(0));
    let addr = start_mock_server(
        cancel_token.clone(),
        sample_rate_holder.clone(),
        packet_size_holder.clone(),
        None,
        None,
    )
    .await?;
    let url = format!("ws://127.0.0.1:{}/", addr.port());
    let track = create_track(
        track_id.clone(),
        url,
        sending_sample_rate,
        receiving_sample_rate,
        packet_size,
        CancellationToken::new(),
        None,
    )?;
    let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(10);
    let (packet_sender, mut packet_receiver) = tokio::sync::mpsc::unbounded_channel();

    track.start(event_sender, packet_sender).await?;

    for i in 0..50 {
        let slice = sample[i * 160..(i + 1) * 160].to_vec();
        let audio_frame = AudioFrame {
            track_id: track_id.clone(),
            samples: Samples::PCM { samples: slice },
            timestamp: crate::media::get_timestamp(),
            sample_rate: sending_sample_rate,
        };
        track.send_packet(&audio_frame).await?;
    }

    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        receiving_sample_rate,
        sample_rate_holder.load(Ordering::Relaxed)
    );
    assert_eq!(packet_size, packet_size_holder.load(Ordering::Relaxed));

    track.stop().await?;

    let mut buffer = Vec::with_capacity(16000);
    while let Some(packet) = packet_receiver.recv().await {
        if let Samples::PCM { samples } = packet.samples {
            buffer.extend_from_slice(samples.as_slice());
        }
    }
    assert_eq!(sample.len() * 2, buffer.len());

    let mut ended = false;
    while let Ok(event) = event_receiver.recv().await {
        if let SessionEvent::TrackEnd { .. } = event {
            ended = true;
            break;
        }
    }
    assert!(ended);

    Ok(())
}

#[tokio::test]
async fn test_track_end_on_cancel() -> Result<()> {
    let track_id = "cancel_test_track".to_string();
    let sending_sample_rate = 8000;
    let receiving_sample_rate = 16000;
    let packet_size = 640;
    let cancel_token = CancellationToken::new();
    let sample_rate_holder = Arc::new(AtomicU32::new(0));
    let packet_size_holder = Arc::new(AtomicU32::new(0));
    let server_cancel_token = CancellationToken::new();
    let addr = start_mock_server(
        server_cancel_token.clone(),
        sample_rate_holder.clone(),
        packet_size_holder.clone(),
        None,
        None,
    )
    .await?;
    let url = format!("ws://127.0.0.1:{}/", addr.port());
    let track = create_track(
        track_id.clone(),
        url,
        sending_sample_rate,
        receiving_sample_rate,
        packet_size,
        cancel_token.clone(),
        None,
    )?;
    let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(10);
    let (packet_sender, _packet_receiver) = tokio::sync::mpsc::unbounded_channel();

    track.start(event_sender, packet_sender).await?;

    // Wait a bit for connection to establish
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cancel the token
    cancel_token.cancel();

    // Wait for TrackEnd event
    let mut track_end_received = false;
    let timeout = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(event) = event_receiver.recv().await {
            if let SessionEvent::TrackEnd {
                track_id: event_track_id,
                ..
            } = event
            {
                if event_track_id == track_id {
                    track_end_received = true;
                    break;
                }
            }
        }
    })
    .await;

    assert!(
        track_end_received,
        "TrackEnd event should be sent when cancel_token is cancelled"
    );
    assert!(timeout.is_ok(), "Should receive TrackEnd within timeout");

    server_cancel_token.cancel();
    Ok(())
}

#[tokio::test]
async fn test_resampling_to_output_sample_rate() -> Result<()> {
    let track_id = "resample_test_track".to_string();
    let input_sample_rate = 8000;
    let output_sample_rate = 16000; // Different from input
    let packet_size = 640;
    let cancel_token = CancellationToken::new();
    let sample_rate_holder = Arc::new(AtomicU32::new(0));
    let packet_size_holder = Arc::new(AtomicU32::new(0));
    let received_data = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let received_timestamps = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let server_cancel_token = CancellationToken::new();

    let addr = start_mock_server(
        server_cancel_token.clone(),
        sample_rate_holder.clone(),
        packet_size_holder.clone(),
        Some(received_data.clone()),
        Some(received_timestamps.clone()),
    )
    .await?;
    let url = format!("ws://127.0.0.1:{}/", addr.port());
    let track = create_track(
        track_id.clone(),
        url,
        input_sample_rate,
        output_sample_rate,
        packet_size,
        cancel_token.clone(),
        None,
    )?;
    let (event_sender, _event_receiver) = tokio::sync::broadcast::channel(10);
    let (packet_sender, _packet_receiver) = tokio::sync::mpsc::unbounded_channel();

    track.start(event_sender, packet_sender).await?;

    // Wait for connection
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Generate 1 second of audio at input_sample_rate
    let sample = generate_audio_samples(input_sample_rate);

    // Send audio frames with input_sample_rate
    for i in 0..50 {
        let slice = sample[i * 160..(i + 1) * 160].to_vec();
        let audio_frame = AudioFrame {
            track_id: track_id.clone(),
            samples: Samples::PCM { samples: slice },
            timestamp: crate::media::get_timestamp(),
            sample_rate: input_sample_rate, // Different from output_sample_rate
        };
        track.send_packet(&audio_frame).await?;
    }

    // Wait for data to be sent
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify the received data
    let data_guard = received_data.lock().await;
    assert!(!data_guard.is_empty(), "Should receive data from websocket");

    // Convert received bytes to samples
    let received_samples = bytes_to_samples(&data_guard);

    // Calculate expected number of samples after resampling
    // Input: 50 * 160 = 8000 samples at 8000Hz = 1 second
    // Output: should be 16000 samples at 16000Hz = 1 second (2x resampling)
    let expected_samples = (50 * 160) * output_sample_rate / input_sample_rate;

    // Allow some tolerance for buffering/packet boundaries
    let tolerance = (expected_samples as f64 * 0.1) as u32;
    let received_len = received_samples.len() as u32;
    assert!(
        received_len >= expected_samples.saturating_sub(tolerance),
        "Received {} samples, expected at least {} (after resampling from {}Hz to {}Hz)",
        received_len,
        expected_samples.saturating_sub(tolerance),
        input_sample_rate,
        output_sample_rate
    );

    // Verify sample_rate parameter was set correctly
    assert_eq!(
        output_sample_rate,
        sample_rate_holder.load(Ordering::Relaxed),
        "WebSocket should receive correct output_sample_rate parameter"
    );

    cancel_token.cancel();
    server_cancel_token.cancel();
    Ok(())
}

#[tokio::test]
async fn test_ptime_buffering() -> Result<()> {
    use crate::media::codecs::samples_to_bytes;

    let track_id = "ptime_test_track".to_string();
    let input_sample_rate = 16000;
    let output_sample_rate = 16000;
    let packet_size = 3200;
    let ptime_ms = 20; // 20ms ptime
    let cancel_token = CancellationToken::new();
    let sample_rate_holder = Arc::new(AtomicU32::new(0));
    let packet_size_holder = Arc::new(AtomicU32::new(0));
    let server_cancel_token = CancellationToken::new();

    // Generate audio samples and convert to bytes
    let sample = generate_audio_samples(input_sample_rate);
    let audio_bytes = samples_to_bytes(&sample);

    let addr = start_sending_server(
        server_cancel_token.clone(),
        sample_rate_holder.clone(),
        packet_size_holder.clone(),
        audio_bytes,
    )
    .await?;
    let url = format!("ws://127.0.0.1:{}/", addr.port());
    let track = create_track(
        track_id.clone(),
        url,
        input_sample_rate,
        output_sample_rate,
        packet_size,
        cancel_token.clone(),
        Some(ptime_ms),
    )?;
    let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(10);
    let (packet_sender, mut packet_receiver) = tokio::sync::mpsc::unbounded_channel();

    track.start(event_sender, packet_sender).await?;

    // Wait for connection and data to be sent
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Collect all emitted packets with timestamps
    // Use the timestamp from AudioFrame which is set when emitted (in milliseconds)
    let mut emitted_packets = Vec::new();
    let start_time = Instant::now();

    // Collect packets for a reasonable duration
    let collect_duration = Duration::from_millis(ptime_ms as u64 * 5);
    while start_time.elapsed() < collect_duration {
        if let Ok(packet) = packet_receiver.try_recv() {
            emitted_packets.push((packet.timestamp, packet));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Also try to get any remaining packets
    while let Ok(packet) = packet_receiver.try_recv() {
        emitted_packets.push((packet.timestamp, packet));
    }

    assert!(
        emitted_packets.len() > 0,
        "Should emit audio frames when ptime is set and data is received from websocket"
    );

    // Verify samples are emitted in ptime-sized chunks
    // For 16000Hz, 20ms = 320 samples
    let expected_samples = (input_sample_rate * ptime_ms / 1000) as usize;
    for (_, packet) in &emitted_packets {
        if let Samples::PCM { samples } = &packet.samples {
            assert_eq!(
                samples.len(),
                expected_samples,
                "Emitted audio should be in ptime-sized chunks ({} samples for {}ms at {}Hz)",
                expected_samples,
                ptime_ms,
                input_sample_rate
            );
        }
    }

    // Verify timestamps show periodic emission (if we have multiple packets)
    // Timestamps are in milliseconds (u64), so we can subtract them directly
    let ptime_duration_ms = ptime_ms as u64;
    let tolerance_ms = 10u64; // 10ms tolerance for test timing

    if emitted_packets.len() >= 2 {
        for i in 1..emitted_packets.len() {
            let interval_ms = emitted_packets[i]
                .0
                .saturating_sub(emitted_packets[i - 1].0);
            // Allow some tolerance for timing - should be approximately ptime_ms
            assert!(
                interval_ms >= ptime_duration_ms.saturating_sub(tolerance_ms)
                    && interval_ms <= ptime_duration_ms + tolerance_ms * 2,
                "Audio should be emitted at ptime intervals ({}ms), got {}ms",
                ptime_ms,
                interval_ms
            );
        }
    }

    cancel_token.cancel();

    // Wait for TrackEnd
    let mut track_end_received = false;
    let timeout = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(event) = event_receiver.recv().await {
            if let SessionEvent::TrackEnd { .. } = event {
                track_end_received = true;
                break;
            }
        }
    })
    .await;

    assert!(track_end_received, "TrackEnd should be sent");
    assert!(timeout.is_ok(), "Should receive TrackEnd within timeout");

    server_cancel_token.cancel();
    Ok(())
}
