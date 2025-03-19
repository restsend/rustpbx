use crate::media::{jitter::JitterBuffer, processor::AudioFrame};
use std::time::Duration;

#[test]
fn test_jitter_buffer_in_order_packets() {
    let mut buffer = JitterBuffer::new(16000); // 16kHz sample rate

    // Add packets in order
    buffer.push(
        0,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![1.0, 2.0, 3.0],
            timestamp: 0,
            sample_rate: 16000,
            channels: 1,
        },
    );
    buffer.push(
        20,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![4.0, 5.0, 6.0],
            timestamp: 20,
            sample_rate: 16000,
            channels: 1,
        },
    );
    buffer.push(
        40,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![7.0, 8.0, 9.0],
            timestamp: 40,
            sample_rate: 16000,
            channels: 1,
        },
    );

    // Check packets come out in order
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 0);
    assert_eq!(frame.samples, vec![1.0, 2.0, 3.0]);

    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 20);
    assert_eq!(frame.samples, vec![4.0, 5.0, 6.0]);

    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 40);
    assert_eq!(frame.samples, vec![7.0, 8.0, 9.0]);
}

#[test]
fn test_jitter_buffer_out_of_order_packets() {
    let mut buffer = JitterBuffer::new(16000);

    // Add packets out of order
    buffer.push(
        20,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![4.0, 5.0, 6.0],
            timestamp: 20,
            sample_rate: 16000,
            channels: 1,
        },
    );
    buffer.push(
        0,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![1.0, 2.0, 3.0],
            timestamp: 0,
            sample_rate: 16000,
            channels: 1,
        },
    );
    buffer.push(
        40,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![7.0, 8.0, 9.0],
            timestamp: 40,
            sample_rate: 16000,
            channels: 1,
        },
    );

    // Check packets come out in order
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 0);
    assert_eq!(frame.samples, vec![1.0, 2.0, 3.0]);

    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 20);
    assert_eq!(frame.samples, vec![4.0, 5.0, 6.0]);

    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 40);
    assert_eq!(frame.samples, vec![7.0, 8.0, 9.0]);
}

#[test]
fn test_jitter_buffer_late_packets() {
    let mut buffer = JitterBuffer::new(16000);

    // Add some packets
    buffer.push(
        0,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![1.0, 2.0, 3.0],
            timestamp: 0,
            sample_rate: 16000,
            channels: 1,
        },
    );
    buffer.push(
        20,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![4.0, 5.0, 6.0],
            timestamp: 20,
            sample_rate: 16000,
            channels: 1,
        },
    );

    // Pop first packet
    let packet = buffer.pop();
    assert!(packet.is_some());
    let frame = packet.unwrap();
    assert_eq!(frame.timestamp, 0);

    // Add a late packet (timestamp 0 again) - should be ignored
    buffer.push(
        0,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![1.0, 2.0, 3.0],
            timestamp: 0,
            sample_rate: 16000,
            channels: 1,
        },
    );

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
    buffer.push(
        0,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![1.0, 2.0, 3.0],
            timestamp: 0,
            sample_rate: 16000,
            channels: 1,
        },
    );
    buffer.push(
        40,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![7.0, 8.0, 9.0],
            timestamp: 40,
            sample_rate: 16000,
            channels: 1,
        },
    );

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
        let ts = (i * 20) as u64;
        buffer.push(
            ts,
            AudioFrame {
                track_id: "test".to_string(),
                samples: vec![1.0, 2.0, 3.0],
                timestamp: ts as u32,
                sample_rate: 16000,
                channels: 1,
            },
        );
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
    buffer.push(
        0,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![1.0, 2.0, 3.0],
            timestamp: 0,
            sample_rate: 16000,
            channels: 1,
        },
    );
    buffer.push(
        20,
        AudioFrame {
            track_id: "test".to_string(),
            samples: vec![4.0, 5.0, 6.0],
            timestamp: 20,
            sample_rate: 16000,
            channels: 1,
        },
    );

    // Clear buffer
    buffer.clear();

    // Buffer should be empty
    assert!(buffer.is_empty());

    // No packets should be available
    let packet = buffer.pop();
    assert!(packet.is_none());
}
