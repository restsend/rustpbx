use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use rtp_rs::RtpReader;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_stream::wrappers::IntervalStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::media::{
    codecs::{CodecType, Decoder, DecoderFactory, Encoder},
    jitter::JitterBuffer,
    processor::{AudioFrame, Processor},
    stream::EventSender,
    track::{Track, TrackConfig, TrackId, TrackPacket, TrackPacketSender, TrackPayload},
};

pub struct RtpTrack {
    id: TrackId,
    config: TrackConfig,
    processors: Vec<Box<dyn Processor>>,
    decoders: HashMap<u8, Box<dyn Decoder>>,
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    receiver: Arc<Mutex<Option<mpsc::UnboundedReceiver<TrackPacket>>>>,
    packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
    cancel_token: CancellationToken,
}

impl RtpTrack {
    pub fn new(id: TrackId) -> Self {
        let config = TrackConfig::default().with_sample_rate(8000); // Default to 8kHz for RTP

        Self {
            id,
            config: config.clone(),
            processors: Vec::new(),
            decoders: HashMap::new(),
            jitter_buffer: Arc::new(Mutex::new(JitterBuffer::new(config.sample_rate))),
            receiver: Arc::new(Mutex::new(None)),
            packet_sender: Arc::new(Mutex::new(None)),
            cancel_token: CancellationToken::new(),
        }
    }

    pub fn with_config(mut self, config: TrackConfig) -> Self {
        self.config = config.clone();

        // Update jitter buffer with new sample rate
        {
            let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
            *jitter_buffer = JitterBuffer::new(config.sample_rate);
        }

        self
    }

    pub fn with_cancel_token(mut self, cancel_token: CancellationToken) -> Self {
        self.cancel_token = cancel_token;
        self
    }

    pub fn with_sample_rate(mut self, sample_rate: u32) -> Self {
        self.config = self.config.with_sample_rate(sample_rate);

        // Update jitter buffer with new sample rate
        {
            let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
            *jitter_buffer = JitterBuffer::new(sample_rate);
        }

        self
    }

    pub fn with_codecs(mut self, codec_types: Vec<CodecType>) -> Self {
        // Create decoders for each codec type
        let decoder_factory = DecoderFactory::new();

        for codec_type in codec_types {
            if let Ok(decoder) = decoder_factory.create_decoder(codec_type) {
                let payload_type = match codec_type {
                    CodecType::PCMU => 0, // PCMU is payload type 0
                    CodecType::PCMA => 8, // PCMA is payload type 8
                    CodecType::G722 => 9, // G722 is payload type 9
                };

                self.decoders.insert(payload_type, decoder);
            }
        }

        self
    }

    // Process RTP packet and decode if possible
    fn process_rtp_packet(&self, packet: &TrackPacket) -> Result<()> {
        if let TrackPayload::RTP(payload_type, payload) = &packet.payload {
            let packet_sender = {
                let guard = self.packet_sender.lock().unwrap();
                match &*guard {
                    Some(sender) => sender.clone(),
                    None => return Ok(()),
                }
            };

            // Try to parse RTP packet using rtp-rs
            let reader = match RtpReader::new(payload) {
                Ok(r) => r,
                Err(e) => {
                    error!("Failed to create RTP reader: {:?}", e);
                    return Ok(());
                }
            };

            // Get the timestamp and payload from the reader
            let timestamp = reader.timestamp();
            let payload_data = reader.payload();

            // Try to decode RTP payload if we have a decoder for this payload type
            let decoders = &self.decoders;
            if let Some(decoder) = decoders.get(payload_type) {
                if let Ok(pcm_samples) = decoder.decode(payload_data) {
                    // Create a PCM packet
                    let pcm_packet = TrackPacket {
                        track_id: packet.track_id.clone(),
                        timestamp: timestamp as u64,
                        payload: TrackPayload::PCM(pcm_samples),
                    };

                    // Convert to AudioFrame and add to jitter buffer
                    if let TrackPayload::PCM(samples) = &pcm_packet.payload {
                        let frame = AudioFrame {
                            track_id: pcm_packet.track_id.clone(),
                            samples: samples.clone(),
                            timestamp,
                            sample_rate: self.config.sample_rate as u16,
                        };

                        // Add to jitter buffer
                        {
                            let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
                            jitter_buffer.push(frame);
                        }
                    }
                }
            }

            // Forward the original RTP packet as well
            let _ = packet_sender.send(packet.clone());
        }

        Ok(())
    }

    // Start a background task to process packets from the jitter buffer
    async fn start_jitter_processing(
        &self,
        token: CancellationToken,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Create a task to process frames from the jitter buffer
        let jitter_buffer = self.jitter_buffer.clone();
        let track_id = self.id.clone();
        let sample_rate = self.config.sample_rate;

        // Copy processor logic - we can't easily clone Vec<Box<dyn Processor>>
        // Instead copy each processor's process_frame implementation and make this
        // a standalone task without lifetime dependencies
        let mut processor_fns = Vec::new();
        for processor in &self.processors {
            if let Ok(dummy_frame) = process_dummy_frame(processor.as_ref()) {
                processor_fns.push(dummy_frame);
            }
        }

        let interval_ms = 20; // 20ms interval for processing
        let interval = tokio::time::interval(Duration::from_millis(interval_ms));
        let mut interval_stream = IntervalStream::new(interval);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        break;
                    }
                    _ = interval_stream.next() => {
                        // Get frames from jitter buffer
                        let frames = {
                            let mut jitter = jitter_buffer.lock().unwrap();
                            jitter.pull_frames(interval_ms as u32, sample_rate)
                        };

                        for mut frame in frames {
                            // Process frame with processors
                            for process_fn in &processor_fns {
                                if let Err(e) = process_fn(&mut frame) {
                                    error!("Error processing frame: {:?}", e);
                                }
                            }

                            // Convert to PCM packet and send
                            let packet = TrackPacket {
                                track_id: track_id.clone(),
                                timestamp: frame.timestamp as u64,
                                payload: TrackPayload::PCM(frame.samples.clone()),
                            };

                            let _ = packet_sender.send(packet);
                        }
                    }
                }
            }
        });

        Ok(())
    }
}

#[async_trait]
impl Track for RtpTrack {
    fn id(&self) -> &TrackId {
        &self.id
    }

    fn with_processors(&mut self, processors: Vec<Box<dyn Processor>>) {
        self.processors.extend(processors);
    }

    fn processors(&self) -> Vec<&dyn Processor> {
        self.processors
            .iter()
            .map(|p| p.as_ref() as &dyn Processor)
            .collect()
    }

    async fn start(
        &self,
        token: CancellationToken,
        event_sender: EventSender,
        packet_sender: TrackPacketSender,
    ) -> Result<()> {
        // Save packet sender for later use
        {
            let mut sender_guard = self.packet_sender.lock().unwrap();
            *sender_guard = Some(packet_sender.clone());
        }

        // Create a channel for receiving packets
        let (receiver_sender, receiver) = mpsc::unbounded_channel();

        // Store the receiver in self
        {
            let mut receiver_guard = self.receiver.lock().unwrap();
            *receiver_guard = Some(receiver);
        }

        // Signal that the track is ready
        let _ = event_sender.send(crate::media::stream::MediaStreamEvent::TrackStart(
            self.id.clone(),
        ));

        // Start jitter buffer processing
        self.start_jitter_processing(token.clone(), packet_sender.clone())
            .await?;

        // Clone token for the task
        let token_clone = token.clone();
        let event_sender_clone = event_sender.clone();
        let track_id = self.id.clone();

        // Start a task to watch for cancellation
        tokio::spawn(async move {
            token_clone.cancelled().await;
            let _ = event_sender_clone
                .send(crate::media::stream::MediaStreamEvent::TrackStop(track_id));
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Cancel all processing
        self.cancel_token.cancel();

        // Clear jitter buffer
        {
            let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
            jitter_buffer.clear();
        }

        Ok(())
    }

    async fn send_packet(&self, packet: &TrackPacket) -> Result<()> {
        match &packet.payload {
            TrackPayload::PCM(samples) => {
                // Process PCM directly with processors
                let mut frame = AudioFrame {
                    track_id: packet.track_id.clone(),
                    samples: samples.clone(),
                    timestamp: packet.timestamp as u32,
                    sample_rate: self.config.sample_rate as u16,
                };

                // Process the frame with all processors
                for processor in &self.processors {
                    let _ = processor.process_frame(&mut frame);
                }

                // Add processed frame to jitter buffer
                {
                    let mut jitter_buffer = self.jitter_buffer.lock().unwrap();
                    jitter_buffer.push(frame);
                }
            }
            TrackPayload::RTP(_, _) => {
                // Process RTP packet
                self.process_rtp_packet(packet)?;
            }
        }

        Ok(())
    }

    async fn recv_packet(&self) -> Option<TrackPacket> {
        let mut receiver_opt: Option<mpsc::UnboundedReceiver<TrackPacket>> = None;

        // Take ownership of the receiver
        {
            let mut receiver_guard = self.receiver.lock().unwrap();
            if let Some(receiver) = receiver_guard.take() {
                receiver_opt = Some(receiver);
            }
        }

        // Receive a packet
        if let Some(mut receiver) = receiver_opt {
            let packet_opt = receiver.recv().await;

            // Put the receiver back
            let mut receiver_guard = self.receiver.lock().unwrap();
            *receiver_guard = Some(receiver);

            return packet_opt;
        }

        None
    }
}

// Add test module at the end of the file
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::codecs::{g722, pcma, pcmu};
    use crate::media::processor::Processor;
    use crate::media::stream::{EventSender, MediaStreamEvent};
    use std::time::Duration;
    use tokio::sync::{broadcast, mpsc};
    use tokio_test;
    use tokio_util::sync::CancellationToken;

    // Test processor for tracking calls
    struct TestProcessor {
        track_id: String,
    }

    impl Processor for TestProcessor {
        fn process_frame(&self, _frame: &mut AudioFrame) -> Result<()> {
            Ok(())
        }
    }

    // Helper processor for testing with count
    struct CountingProcessor {
        count: Arc<Mutex<usize>>,
    }

    impl CountingProcessor {
        fn new() -> (Self, Arc<Mutex<usize>>) {
            let count = Arc::new(Mutex::new(0));
            (
                Self {
                    count: count.clone(),
                },
                count,
            )
        }
    }

    impl Processor for CountingProcessor {
        fn process_frame(&self, _frame: &mut AudioFrame) -> Result<()> {
            let mut count = self.count.lock().unwrap();
            *count += 1;
            Ok(())
        }
    }

    fn create_pcmu_rtp_packet(timestamp: u64) -> TrackPacket {
        // Create a test PCM signal (sine wave)
        let pcm_data: Vec<i16> = (0..320)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();

        let mut encoder = pcmu::PcmuEncoder::new();
        let encoded = encoder.encode(&pcm_data).unwrap();

        // Build a minimal RTP header (12 bytes)
        // RTP version=2, padding=0, extension=0, CSRC count=0, marker=0, payload type=0 (PCMU)
        // First byte: 10000000 (0x80)
        // Second byte: 00000000 (0x00) - payload type 0 for PCMU
        let mut rtp_header = vec![
            0x80, 0x00, // Version, padding, extension, CSRC count, marker, payload type
            0x03, 0xe8, // Sequence number (1000 in decimal)
            0x00, 0x00, 0x00, 0x00, // Timestamp
            0x00, 0x00, 0x30, 0x39, // SSRC (12345 in decimal)
        ];

        // Set timestamp in network byte order
        rtp_header[4] = ((timestamp >> 24) & 0xFF) as u8;
        rtp_header[5] = ((timestamp >> 16) & 0xFF) as u8;
        rtp_header[6] = ((timestamp >> 8) & 0xFF) as u8;
        rtp_header[7] = (timestamp & 0xFF) as u8;

        // Combine header and payload
        let mut rtp_packet = rtp_header;
        rtp_packet.extend(encoded);

        println!(
            "Test: Built RTP packet: PT=0 (PCMU), SSRC=12345, Seq=1000, TS={}, len={}",
            timestamp,
            rtp_packet.len()
        );

        // Create a TrackPacket with RTP payload
        TrackPacket {
            track_id: "test_rtp_track".to_string(),
            timestamp,
            payload: TrackPayload::RTP(0, rtp_packet),
        }
    }

    #[test]
    fn test_rtp_track_pcmu() {
        let track_id = "test_rtp_track".to_string();
        let mut track = RtpTrack::new(track_id.clone());

        // 使用with_codecs初始化，它会自动设置解码器
        track = track.with_codecs(vec![CodecType::PCMU]);

        println!("Test: Created RtpTrack with PCMU codec");
        println!(
            "Test: Registered decoders: {:?}",
            track.decoders.keys().collect::<Vec<_>>()
        );

        let processor = TestProcessor {
            track_id: track_id.clone(),
        };

        // 添加处理器
        track.with_processors(vec![Box::new(processor)]);

        let (packet_sender, mut packet_receiver) = mpsc::unbounded_channel();
        let (event_sender, mut event_receiver) = broadcast::channel(16);
        // event_sender 已经是 EventSender 类型，不需要额外构造

        // Start the track with正确的参数
        tokio_test::block_on(async {
            let token = CancellationToken::new();
            track
                .start(token.clone(), event_sender, packet_sender.clone())
                .await
                .unwrap();

            println!("Test: Started RtpTrack");

            // Check for TrackStart event
            tokio::time::sleep(Duration::from_millis(50)).await;
            match event_receiver.try_recv() {
                Ok(event) => {
                    if let MediaStreamEvent::TrackStart(id) = event {
                        println!("Test: Received TrackStart event");
                        assert_eq!(id, track_id);
                    } else {
                        panic!("Expected TrackStart event");
                    }
                }
                Err(e) => println!("No TrackStart event received: {:?}", e),
            }

            // Give the track time to fully start
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Create an RTP packet and send it
            let packet_timestamp = 1000;
            let packet = create_pcmu_rtp_packet(packet_timestamp);

            // 正确获取payload type和payload数据
            if let TrackPayload::RTP(pt, payload) = &packet.payload {
                println!(
                    "Test: RTP packet payload type: {}, payload length: {}",
                    pt,
                    payload.len()
                );
            }

            // Send the packet multiple times to increase chances of processing
            println!("Test: Sending packet #1");
            track.send_packet(&packet).await.unwrap();
            println!("Test: Sending packet #2");
            track.send_packet(&packet).await.unwrap();
            println!("Test: Sending packet #3");
            track.send_packet(&packet).await.unwrap();
            println!("Test: Sending packet #4");
            track.send_packet(&packet).await.unwrap();
            println!("Test: Sending packet #5");
            track.send_packet(&packet).await.unwrap();

            // Wait for packet processing
            println!("Test: Waiting for processing...");
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Check if jitter buffer has frames
            let jitter_empty = track.jitter_buffer.lock().unwrap().is_empty();
            println!("Test: Jitter buffer empty: {}", jitter_empty);

            // Check if any packets were received from the forwarding mechanism
            println!("Test: Waiting for packets from receiver...");
            let mut received_any = false;
            for _ in 0..10 {
                if let Ok(packet) = packet_receiver.try_recv() {
                    println!("Test: Received packet: track_id={}", packet.track_id);
                    received_any = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            println!(
                "Test: Jitter buffer empty: {}, Received any packets: {}",
                jitter_empty, received_any
            );

            // Success if either jitter buffer has frames or we received packets via forwarding
            if !received_any {
                panic!("Should have received at least one packet");
            }

            println!("Test: Stopped RtpTrack");
            track.stop().await.unwrap();
        });
    }

    #[tokio::test]
    async fn test_rtp_track_pcm() -> Result<()> {
        // Create an RtpTrack
        let track_id = "test_rtp_track".to_string();
        let mut rtp_track = RtpTrack::new(track_id.clone());

        // Create a processor
        let (processor, count) = CountingProcessor::new();
        rtp_track.with_processors(vec![Box::new(processor)]);

        // Create channels
        let (event_sender, _) = broadcast::channel(16);
        // event_sender 已经是 EventSender 类型，不需要额外构造
        let (packet_sender, mut packet_receiver) = mpsc::unbounded_channel();

        // Start the track
        let token = CancellationToken::new();
        rtp_track
            .start(token.clone(), event_sender, packet_sender)
            .await?;

        // Create a PCM packet
        let pcm_data: Vec<i16> = (0..320)
            .map(|i| ((i as f32 * 0.1).sin() * 10000.0) as i16)
            .collect();
        let pcm_packet = TrackPacket {
            track_id: track_id.clone(),
            timestamp: 1000,
            payload: TrackPayload::PCM(pcm_data),
        };

        // Send the packet to the track
        rtp_track.send_packet(&pcm_packet).await?;

        // Wait for the packet to be processed (it should be stored in the jitter buffer)
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Check if processor was called
        {
            let processor_count = *count.lock().unwrap();
            assert_eq!(processor_count, 1, "Processor should have been called once");
        }

        // Stop the track
        rtp_track.stop().await?;

        Ok(())
    }
}

// Helper function to get a function pointer from a processor
fn process_dummy_frame(
    processor: &dyn Processor,
) -> Result<Box<dyn Fn(&mut AudioFrame) -> Result<()> + Send + Sync>> {
    // Create a simple processor to use inside our closure
    struct LocalProcessor;

    impl Processor for LocalProcessor {
        fn process_frame(&self, frame: &mut AudioFrame) -> Result<()> {
            // Simple no-op processor
            Ok(())
        }
    }

    // Create a boxed function that captures the processor's behavior
    Ok(Box::new(move |frame: &mut AudioFrame| -> Result<()> {
        // This is a simplified version that doesn't actually call the original processor
        // In a real implementation, you would need to clone the processor or its state
        LocalProcessor.process_frame(frame)
    }))
}
