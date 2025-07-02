use anyhow::Result;
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, SizedSample, StreamConfig};
use dotenv::dotenv;
use futures::StreamExt;
use rustpbx::llm::LlmContent;
use rustpbx::media::codecs::bytes_to_samples;
use rustpbx::media::track::file::read_wav_file;
use rustpbx::synthesis::SynthesisClient;
use rustpbx::transcription::TencentCloudAsrClientBuilder;
use rustpbx::{PcmBuf, Sample};
use std::collections::VecDeque;
/// This is a demo application which reads wav file from command line
/// and processes it with ASR, LLM and TTS. It also supports local microphone input
/// and speaker output with Voice Activity Detection (VAD) and Noise Suppression (NS).
///
/// ```bash
/// # Use a WAV file as input and write to a WAV file
/// cargo run --example voice_demo -- --input fixtures/hello_book_course_zh_16k.wav --output out.wav
///
/// # Use microphone input and speaker output
/// cargo run --example voice_demo
/// ```
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber;

use rustpbx::{
    event::SessionEvent,
    llm::{LlmClient, OpenAiClientBuilder},
    media::codecs::resample,
    synthesis::{SynthesisOption, TencentCloudTtsClient},
    transcription::{TranscriptionClient, TranscriptionOption},
};

/// Audio processing buffer for handling real-time audio samples
struct AudioBuffer {
    buffer: Mutex<VecDeque<Sample>>,
    max_size: usize,
}

impl AudioBuffer {
    fn new(max_size: usize) -> Self {
        Self {
            buffer: Mutex::new(VecDeque::with_capacity(max_size)),
            max_size,
        }
    }

    fn push(&self, samples: &[Sample]) {
        let mut buffer = self.buffer.lock().unwrap();
        for &sample in samples {
            buffer.push_back(sample);
            if buffer.len() > self.max_size {
                buffer.pop_front();
            }
        }
    }

    fn drain(&self, max_samples: usize) -> PcmBuf {
        let mut buffer = self.buffer.lock().unwrap();
        let count = std::cmp::min(max_samples, buffer.len());
        let mut result = Vec::with_capacity(count);
        for _ in 0..count {
            if let Some(sample) = buffer.pop_front() {
                result.push(sample);
            }
        }
        result
    }
}

/// Audio Agent
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    #[arg(short, long)]
    input: Option<String>,
    #[arg(short = 'R', long, default_value = "16000")]
    sample_rate: u32,
    /// System prompt for LLM
    #[arg(
        short = 'p',
        long,
        default_value = "You are a helpful assistant who responds in Chinese."
    )]
    prompt: String,
    /// Enable VAD (Voice Activity Detection)
    #[arg(long, default_value = "true")]
    vad: bool,

    /// Enable NS (Noise Suppression)
    #[arg(long, default_value = "true")]
    ns: bool,
}
// Build an input stream for capturing audio from a microphone
fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    audio_buffer: Arc<AudioBuffer>,
    audio_sender: mpsc::Sender<PcmBuf>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + SizedSample + 'static,
{
    let err_fn = move |err| {
        error!("Error on input stream: {}", err);
    };

    match std::any::type_name::<T>() {
        "i16" => {
            let stream = device.build_input_stream(
                config,
                move |data: &[Sample], _: &cpal::InputCallbackInfo| {
                    // Already i16, just need to clone
                    let samples_i16 = data.to_vec();
                    audio_buffer.push(&samples_i16);
                    let _ = audio_sender.try_send(samples_i16);
                },
                err_fn,
                None,
            )?;
            Ok(stream)
        }
        "f32" => {
            let stream = device.build_input_stream(
                config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // Convert f32 to i16
                    let samples_i16: PcmBuf = data
                        .iter()
                        .map(|&sample| (sample * 32767.0).round() as i16)
                        .collect();
                    audio_buffer.push(&samples_i16);
                    let _ = audio_sender.try_send(samples_i16);
                },
                err_fn,
                None,
            )?;
            Ok(stream)
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Unsupported sample format: {}",
                std::any::type_name::<T>()
            ));
        }
    }
}

// Build an output stream for playing audio through speakers
fn build_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    audio_buffer: Arc<AudioBuffer>,
    sample_rate: Option<u32>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + SizedSample + 'static,
{
    let err_fn = move |err| {
        error!("Error on output stream: {}", err);
    };

    let config = if let Some(rate) = sample_rate {
        let mut custom_config = config.clone();
        custom_config.sample_rate = cpal::SampleRate(rate);
        custom_config
    } else {
        config.clone()
    };

    match std::any::type_name::<T>() {
        "i16" => {
            let stream = device.build_output_stream(
                &config,
                move |data: &mut [Sample], _: &cpal::OutputCallbackInfo| {
                    let samples = audio_buffer.drain(data.len());
                    for (i, out) in data.iter_mut().enumerate() {
                        *out = if i < samples.len() { samples[i] } else { 0 };
                    }
                },
                err_fn,
                None,
            )?;
            Ok(stream)
        }
        "f32" => {
            let stream = device.build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let samples = audio_buffer.drain(data.len());
                    for (i, out) in data.iter_mut().enumerate() {
                        let value = if i < samples.len() { samples[i] } else { 0 };
                        *out = value as f32 / 32767.0;
                    }
                },
                err_fn,
                None,
            )?;
            Ok(stream)
        }
        _ => {
            return Err(anyhow::anyhow!(
                "Unsupported sample format: {}",
                std::any::type_name::<T>()
            ));
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    dotenv().ok();

    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let cancel_token = CancellationToken::new();

    let audio_buffer = Arc::new(AudioBuffer::new(args.sample_rate as usize * 10)); // 10 seconds at specified sample rate
    let output_buffer = Arc::new(AudioBuffer::new(args.sample_rate as usize * 10));
    let (audio_sender, mut audio_receiver) = mpsc::channel::<PcmBuf>(100);

    let secret_id = match std::env::var("TENCENT_SECRET_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            eprintln!("Error: TENCENT_SECRET_ID environment variable not set or empty.");
            eprintln!("Please set it in .env file or in your environment.");
            return Err(anyhow::anyhow!("Missing TENCENT_SECRET_ID"));
        }
    };

    let secret_key = match std::env::var("TENCENT_SECRET_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            eprintln!("Error: TENCENT_SECRET_KEY environment variable not set or empty.");
            eprintln!("Please set it in .env file or in your environment.");
            return Err(anyhow::anyhow!("Missing TENCENT_SECRET_KEY"));
        }
    };

    let appid = match std::env::var("TENCENT_APPID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            eprintln!("Error: TENCENT_APPID environment variable not set or empty.");
            eprintln!("Please set it in .env file or in your environment.");
            return Err(anyhow::anyhow!("Missing TENCENT_APPID"));
        }
    };
    info!("Found TENCENT_APPID in environment");

    let transcription_config = TranscriptionOption {
        app_id: Some(appid.clone()),
        secret_id: Some(secret_id.clone()),
        secret_key: Some(secret_key.clone()),
        ..Default::default()
    };
    let (event_sender, mut event_receiver) = tokio::sync::broadcast::channel(16);
    let asr_client = Arc::new(
        TencentCloudAsrClientBuilder::new(transcription_config, event_sender)
            .with_cancel_token(cancel_token.clone())
            .build()
            .await?,
    );

    let llm_client = OpenAiClientBuilder::from_env().build()?;
    // Set up TTS pipeline
    let synthesis_config = SynthesisOption {
        app_id: Some(appid),
        secret_id: Some(secret_id),
        secret_key: Some(secret_key),
        ..Default::default()
    };

    let tts_client = TencentCloudTtsClient::new(synthesis_config);

    // Set up microphone input if requested
    let mut input_stream = None;
    let mut input_sample_rate = 16000;
    if args.input.is_none() {
        let host = cpal::default_host();
        let input_device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("No input device available"))?;

        info!("Using input device: {}", input_device.name()?);

        let input_config = input_device.default_input_config()?;
        input_sample_rate = input_config.sample_rate().0;
        let audio_buffer_clone = audio_buffer.clone();
        let audio_sender_clone = audio_sender.clone();

        input_stream = Some(match input_config.sample_format() {
            SampleFormat::I16 => build_input_stream::<i16>(
                &input_device,
                &input_config.into(),
                audio_buffer_clone.clone(),
                audio_sender_clone,
            )?,
            SampleFormat::U16 => build_input_stream::<u16>(
                &input_device,
                &input_config.into(),
                audio_buffer_clone.clone(),
                audio_sender_clone,
            )?,
            SampleFormat::F32 => build_input_stream::<f32>(
                &input_device,
                &input_config.into(),
                audio_buffer_clone.clone(),
                audio_sender_clone,
            )?,
            sample_format => {
                return Err(anyhow::anyhow!(
                    "Unsupported sample format: {:?}",
                    sample_format
                ))
            }
        });

        // Start the input stream
        input_stream.as_ref().unwrap().play()?;
        info!("Microphone input started");
    }

    let host = cpal::default_host();
    let output_device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No output device available"))?;

    info!("Using output device: {}", output_device.name()?);

    let output_config = output_device.default_output_config()?;
    info!("Default output config: {:?}", output_config);
    let sample_rate = args.sample_rate;
    let output_sample_rate = output_config.sample_rate().0;
    let output_buffer_clone = output_buffer.clone();
    let stream_config = StreamConfig {
        channels: 1,
        sample_rate: SampleRate(output_sample_rate),
        buffer_size: BufferSize::Default,
    };
    let output_stream = Some(match output_config.sample_format() {
        SampleFormat::I16 => build_output_stream::<i16>(
            &output_device,
            &stream_config,
            output_buffer_clone.clone(),
            Some(output_sample_rate),
        )?,
        SampleFormat::U16 => build_output_stream::<u16>(
            &output_device,
            &stream_config,
            output_buffer_clone.clone(),
            Some(output_sample_rate),
        )?,
        SampleFormat::F32 => build_output_stream::<f32>(
            &output_device,
            &stream_config,
            output_buffer_clone.clone(),
            Some(output_sample_rate),
        )?,
        sample_format => {
            return Err(anyhow::anyhow!(
                "Unsupported sample format: {:?}",
                sample_format
            ))
        }
    });

    // Start the output stream
    output_stream.as_ref().unwrap().play()?;
    info!(
        "Speaker output started with sample rate {}Hz",
        output_sample_rate
    );
    let cancel_token_clone = cancel_token.clone();
    let audio_input_loop = async move {
        match args.input {
            Some(input_path) => {
                info!("Reading input file: {}", input_path);
                let (samples, sample_rate) =
                    read_wav_file(&input_path).expect("Failed to read wav file");
                info!("Read {} samples at {} Hz", samples.len(), sample_rate);
                let chunk_size = sample_rate as usize / 1000 * 20; // 20ms at 48kHz
                for chunk in samples.chunks(chunk_size) {
                    asr_client.send_audio(chunk).ok();
                    sleep(Duration::from_millis(20)).await;
                }
                asr_client.send_audio(&vec![0; 16000]).ok();
                info!("Input file processed");
                cancel_token_clone.cancelled().await;
            }
            None => {
                info!("Processing microphone audio through pipeline...");
                let chunk_size = args.sample_rate as usize / 1000 * 20; // 20ms at 48kHz
                let mut buffer = Vec::new();
                while let Some(samples) = audio_receiver.recv().await {
                    if input_sample_rate != args.sample_rate {
                        // resample to 16k
                        let resampled_samples =
                            resample::resample_mono(&samples, input_sample_rate, args.sample_rate);
                        buffer.extend_from_slice(&resampled_samples);
                    } else {
                        buffer.extend_from_slice(&samples);
                    }
                    if buffer.len() >= chunk_size {
                        for chunk in buffer.chunks(chunk_size) {
                            asr_client.send_audio(chunk).ok();
                        }
                        buffer.clear();
                    }
                }
            }
        }
    };

    let transcription_loop = async move {
        while let Ok(event) = event_receiver.recv().await {
            match event {
                SessionEvent::AsrFinal { text, .. } => {
                    info!("Transcription: {}", text);
                    let st = Instant::now();
                    match llm_client.generate(&text).await {
                        Ok(mut llm_response) => {
                            while let Some(content) = llm_response.next().await {
                                if let Ok(LlmContent::Final(text)) = content {
                                    info!("LLM response: {}ms {}", st.elapsed().as_millis(), text);
                                    let st = Instant::now();
                                    if let Ok(mut audio_stream) =
                                        tts_client.synthesize(&text, None).await
                                    {
                                        let mut total_bytes = 0;
                                        while let Some(Ok(chunk)) = audio_stream.next().await {
                                            total_bytes += chunk.len();
                                            let audio_data: PcmBuf = bytes_to_samples(&chunk);
                                            let final_audio = if sample_rate != output_sample_rate {
                                                resample::resample_mono(
                                                    &audio_data,
                                                    sample_rate,
                                                    output_sample_rate,
                                                )
                                            } else {
                                                audio_data
                                            };
                                            output_buffer.push(&final_audio);
                                        }
                                        info!(
                                            "TTS synthesis: {}ms ({}) bytes",
                                            st.elapsed().as_millis(),
                                            total_bytes
                                        );
                                    } else {
                                        error!("Error synthesizing TTS");
                                        break;
                                    }
                                } else if let Err(e) = content {
                                    error!("Error generating LLM response: {}", e);
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            error!("Error generating LLM response: {}", e);
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    };

    select! {
        _ = tokio::time::sleep(Duration::from_secs(60)) => {}
        _ = cancel_token.cancelled() => {}
        _ = audio_input_loop => {
            info!("Audio input loop completed");
        }
        _ = transcription_loop => {
            info!("Transcription loop completed");
        }
    };
    // Close input/output streams
    drop(input_stream);
    drop(output_stream);
    info!("All processing complete");

    Ok(())
}
