use crate::media::{
    jitter::JitterBuffer,
    processor::{AudioFrame, AudioPayload},
};

#[test]
fn test_jitter_buffer_in_order_packets() {
    let mut buffer = JitterBuffer::new(16000); // 16kHz sample rate

    // Add packets in order
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![1, 2, 3]),
        ..Default::default()
    });
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![4, 5, 6]),
        timestamp: 20,
        ..Default::default()
    });
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![7, 8, 9]),
        timestamp: 40,
        ..Default::default()
    });

    // Check packets come out in order
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 0);
    let samples = match frame.samples {
        AudioPayload::PCM(samples) => samples,
        _ => panic!("Expected PCM samples"),
    };
    assert_eq!(samples, vec![1, 2, 3]);

    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 20);
    assert!(matches!(frame.samples, AudioPayload::PCM(samples) if samples == vec![4, 5, 6]));

    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 40);
    assert!(matches!(frame.samples, AudioPayload::PCM(samples) if samples == vec![7, 8, 9]));
}

#[test]
fn test_jitter_buffer_out_of_order_packets() {
    let mut buffer = JitterBuffer::new(16000);

    // Add packets out of order
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![4, 5, 6]),
        timestamp: 20,
        ..Default::default()
    });
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![1, 2, 3]),
        timestamp: 0,
        ..Default::default()
    });
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![7, 8, 9]),
        timestamp: 40,
        ..Default::default()
    });

    // Check packets come out in order
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 0);
    assert!(matches!(frame.samples, AudioPayload::PCM(samples) if samples == vec![1, 2, 3]));

    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 20);
    assert!(matches!(frame.samples, AudioPayload::PCM(samples) if samples == vec![4, 5, 6]));

    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 40);
    assert!(matches!(frame.samples, AudioPayload::PCM(samples) if samples == vec![7, 8, 9]));
}

#[test]
fn test_jitter_buffer_late_packets() {
    let mut buffer = JitterBuffer::new(16000);

    // Add some packets
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![1, 2, 3]),
        timestamp: 0,
        ..Default::default()
    });
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![4, 5, 6]),
        timestamp: 20,
        ..Default::default()
    });

    // Pop first packet
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 0);

    // Add a late packet (timestamp 0 again) - should be ignored
    buffer.push(AudioFrame {
        timestamp: 0,
        samples: AudioPayload::PCM(vec![1, 2, 3]),
        ..Default::default()
    });

    // Check buffer length
    assert_eq!(buffer.len(), 1);

    // Next packet should still be timestamp 20
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 20);
}

#[test]
fn test_jitter_buffer_missing_packets() {
    let mut buffer = JitterBuffer::new(16000);

    // Add packets with a gap
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![1, 2, 3]),
        ..Default::default()
    });
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![7, 8, 9]),
        ..Default::default()
    });

    // First packet should come out
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 0);

    // Missing packet should be skipped
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 40);
}

#[test]
fn test_jitter_buffer_overflow() {
    let mut buffer = JitterBuffer::new(16000);

    // Fill buffer with many packets
    for i in 0..1000 {
        let ts = (i * 20) as u32;
        buffer.push(AudioFrame {
            samples: AudioPayload::PCM(vec![1, 2, 3]),
            timestamp: ts,
            ..Default::default()
        });
    }

    // Buffer should not be empty
    assert!(!buffer.is_empty());

    // Oldest packets should be dropped
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert!(frame.timestamp > 0); // First packet should have been dropped
}

#[test]
fn test_jitter_buffer_clear() {
    let mut buffer = JitterBuffer::new(16000);

    // Add some packets
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![1, 2, 3]),
        ..Default::default()
    });
    buffer.push(AudioFrame {
        samples: AudioPayload::PCM(vec![4, 5, 6]),
        ..Default::default()
    });

    // Clear buffer
    buffer.clear();

    // Buffer should be empty
    assert!(buffer.is_empty());

    // No packets should be available
    let packet = buffer.pop();
    assert!(packet.is_none());
}
