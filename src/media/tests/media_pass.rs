use crate::media::{
    media_pass::{MediaPassOption, MediaPassProcessor},
    processor::Processor,
};
use crate::{AudioFrame, Samples};
use futures::StreamExt;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::net::TcpListener;
use tokio::time::Duration;
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{Request, Response},
    },
};
use tokio_util::sync::CancellationToken;

struct TestWebSocketServer {
    addr: SocketAddr,
    received_data_size: Arc<AtomicUsize>,
    received_headers: Arc<Mutex<HashMap<String, String>>>,
}

impl TestWebSocketServer {
    async fn new() -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let received_data_size = Arc::new(AtomicUsize::new(0));
        let received_headers = Arc::new(Mutex::new(HashMap::new()));

        let received_data_size_clone = received_data_size.clone();
        let headers_clone = received_headers.clone();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let received_data_size_clone = received_data_size_clone.clone();
                let headers_clone = headers_clone.clone();

                tokio::spawn(async move {
                    let callback = |request: &Request, response: Response| {
                        let mut headers = headers_clone.lock().unwrap();
                        for (name, value) in request.headers() {
                            if let Ok(value_str) = value.to_str() {
                                headers.insert(name.to_string(), value_str.to_string());
                            }
                        }
                        Ok(response)
                    };

                    if let Ok(ws_stream) = accept_hdr_async(stream, callback).await {
                        let (_ws_sender, mut ws_receiver) = ws_stream.split();
                        while let Some(msg) = ws_receiver.next().await {
                            match msg {
                                Ok(Message::Binary(data)) => {
                                    received_data_size_clone
                                        .fetch_add(data.len(), Ordering::Relaxed);
                                }
                                Ok(Message::Close(_)) => break,
                                Err(_) => break,
                                _ => {}
                            }
                        }
                    }
                });
            }
        });

        Ok(Self {
            addr,
            received_data_size,
            received_headers,
        })
    }

    fn get_received_data_size(&self) -> usize {
        self.received_data_size.load(Ordering::Relaxed)
    }

    fn get_received_headers(&self) -> HashMap<String, String> {
        self.received_headers.lock().unwrap().clone()
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.addr.port())
    }
}

#[tokio::test]
async fn test_real_audio_file_processing() -> anyhow::Result<()> {
    let server = TestWebSocketServer::new().await?;
    let url = server.url();

    let option = MediaPassOption::new(url, 1024);

    let processor = MediaPassProcessor::new(option, CancellationToken::new());

    let (all_samples, sample_rate) =
        crate::media::track::file::read_wav_file("fixtures/sample.wav")?;

    for (i, chunk) in all_samples.chunks(320).enumerate() {
        let mut frame = AudioFrame {
            samples: Samples::PCM {
                samples: chunk.to_vec(),
            },
            sample_rate,
            track_id: "media_pass_test".to_string(),
            timestamp: (i * 20) as u64,
        };

        processor.process_frame(&mut frame)?;
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        all_samples.len() * 2,
        server.get_received_data_size(),
        "Should have received audio data"
    );

    let headers = server.get_received_headers();
    assert_eq!(
        headers
            .get("x-sample-rate")
            .unwrap()
            .parse::<u32>()
            .unwrap(),
        sample_rate,
        "Should have sample rate header"
    );

    assert_eq!(headers.get("x-content-type").unwrap(), "audio/pcm");

    Ok(())
}
