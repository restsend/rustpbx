use crate::AudioFrame;
use crate::Samples;
use crate::event::SessionEvent;
use crate::media::track::Track;
use crate::media::track::media_pass::MediaPassOption;
use crate::media::track::media_pass::MediaPassTrack;
use crate::{PcmBuf, Sample};
use anyhow::Result;
use futures::FutureExt;
use futures::StreamExt;
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
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

            ws.on_upgrade(|websocket| {
                let (tx, rx) = websocket.split();
                rx.forward(tx).map(|result| {
                    if let Err(e) = result {
                        warn!("websocket error: {:?}", e);
                    }
                })
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
) -> Result<MediaPassTrack> {
    let option = MediaPassOption::new(
        url,
        sending_sample_rate,
        receiving_sample_rate,
        Some(packet_size),
    );
    let track = MediaPassTrack::new(0, track_id, cancel_token, option);
    Ok(track)
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
    )?;
    let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(10);
    let (packet_sender, mut packet_receiver) = tokio::sync::mpsc::unbounded_channel();

    track.start(event_sender, packet_sender).await?;

    for i in 0..50 {
        let slice = sample[i * 160..(i + 1) * 160].to_vec();
        let audio_frame = AudioFrame {
            track_id: track_id.clone(),
            samples: Samples::PCM { samples: slice },
            timestamp: crate::get_timestamp(),
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
