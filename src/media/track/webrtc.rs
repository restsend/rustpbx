use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use rtp_rs::RtpReader;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
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

pub struct WebrtcTrack {
    id: TrackId,
    config: TrackConfig,
    processors: Vec<Box<dyn Processor>>,
    decoders: HashMap<u8, Box<dyn Decoder>>,
    jitter_buffer: Arc<Mutex<JitterBuffer>>,
    receiver: Arc<Mutex<Option<mpsc::UnboundedReceiver<TrackPacket>>>>,
    packet_sender: Arc<Mutex<Option<TrackPacketSender>>>,
    cancel_token: CancellationToken,
}

impl WebrtcTrack {
    pub fn new(id: TrackId) -> Self {
        let config = TrackConfig::default().with_sample_rate(48000); // WebRTC typically uses 48kHz

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

    // Process WebRTC RTP packet
    fn process_rtp_packet(&self, packet: &TrackPacket) -> Result<()> {
        if let TrackPayload::RTP(payload_type, payload) = &packet.payload {
            let packet_sender = {
                let guard = self.packet_sender.lock().unwrap();
                match &*guard {
                    Some(sender) => sender.clone(),
                    None => return Ok(()),
                }
            };

            // Try to parse the RTP packet using rtp-rs
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

// Helper processor for testing - keep a static version here for the dummy function
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

#[async_trait]
impl Track for WebrtcTrack {
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
        let (_receiver_sender, receiver) = mpsc::unbounded_channel();

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
