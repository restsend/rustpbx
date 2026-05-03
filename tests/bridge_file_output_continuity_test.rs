use audio_codec::CodecType;
use rustpbx::media::FileTrack;
use rustpbx::media::bridge::{BridgeEndpoint, BridgePeerBuilder};
use rustpbx::media::negotiate::CodecInfo;
use rustrtc::media::{MediaSample, MediaStreamTrack};

fn create_test_wav_file(path: &std::path::Path, sample_count: usize) {
    let sample_rate = 8000u32;
    let bits_per_sample = 16u16;
    let channels = 1u16;
    let data_size = (sample_count * 2) as u32;
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample as u32 / 8);
    let block_align = channels * (bits_per_sample / 8);

    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.extend(std::iter::repeat_n(0u8, data_size as usize));

    std::fs::write(path, wav).expect("failed to write wav file");
}

#[tokio::test]
async fn test_bridge_file_output_preserves_rtp_continuity_across_replacement() {
    let temp_dir = std::env::temp_dir();
    let file_a = temp_dir.join("test_bridge_file_continuity_a.wav");
    let file_b = temp_dir.join("test_bridge_file_continuity_b.wav");
    create_test_wav_file(&file_a, 160);
    create_test_wav_file(&file_b, 160);

    let bridge = BridgePeerBuilder::new("test-bridge-file-continuity".to_string())
        .with_rtp_port_range(25300, 25400)
        .build();
    bridge.setup_bridge().await.unwrap();

    let mk_track = |id: &str, path: &std::path::Path| {
        FileTrack::new(id.to_string())
            .with_path(path.to_string_lossy().to_string())
            .with_loop(false)
            .with_codec_info(CodecInfo {
                payload_type: 0,
                codec: CodecType::PCMU,
                clock_rate: 8000,
                channels: 1,
            })
    };

    bridge
        .replace_output_with_file(BridgeEndpoint::Rtp, &mk_track("a", &file_a))
        .await
        .unwrap();

    let rtp_track = bridge
        .get_rtp_track()
        .await
        .expect("bridge RTP output track should exist");

    let first = tokio::time::timeout(std::time::Duration::from_millis(200), rtp_track.recv())
        .await
        .expect("first frame timeout")
        .expect("first frame should be readable");
    let MediaSample::Audio(first_audio) = first else {
        panic!("expected first audio frame");
    };
    let first_seq = first_audio.sequence_number.expect("first seq should exist");
    let first_ts = first_audio.rtp_timestamp;

    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    bridge
        .replace_output_with_file(BridgeEndpoint::Rtp, &mk_track("b", &file_b))
        .await
        .unwrap();

    let second = tokio::time::timeout(std::time::Duration::from_millis(200), rtp_track.recv())
        .await
        .expect("second frame timeout")
        .expect("second frame should be readable");
    let MediaSample::Audio(second_audio) = second else {
        panic!("expected second audio frame");
    };
    let second_seq = second_audio
        .sequence_number
        .expect("second seq should exist");
    let second_ts = second_audio.rtp_timestamp;

    assert_eq!(
        second_seq,
        first_seq.wrapping_add(1),
        "sequence should continue across file source replacement"
    );
    assert_eq!(
        second_ts,
        first_ts.wrapping_add(160),
        "timestamp should continue with 20ms@8k step across replacement"
    );

    bridge.stop().await;
    let _ = std::fs::remove_file(&file_a);
    let _ = std::fs::remove_file(&file_b);
}
