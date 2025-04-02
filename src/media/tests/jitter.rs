use crate::{media::jitter::JitterBuffer, AudioFrame, Samples};

fn create_test_frame(timestamp: u64) -> AudioFrame {
    AudioFrame {
        track_id: "test".to_string(),
        samples: Samples::Empty,
        timestamp,
        sample_rate: 8000,
    }
}

#[test]
fn test_push_pop_order() {
    let mut buffer = JitterBuffer::new();

    // Push frames in random order
    buffer.push(create_test_frame(30));
    buffer.push(create_test_frame(10));
    buffer.push(create_test_frame(20));

    // Should pop in timestamp order (oldest first)
    assert_eq!(buffer.pop().unwrap().timestamp, 10);
    assert_eq!(buffer.pop().unwrap().timestamp, 20);
    assert_eq!(buffer.pop().unwrap().timestamp, 30);
    assert!(buffer.pop().is_none());
}

#[test]
fn test_max_size() {
    let mut buffer = JitterBuffer::with_max_size(2);

    buffer.push(create_test_frame(10));
    buffer.push(create_test_frame(20));
    buffer.push(create_test_frame(30)); // This should replace the oldest frame (10)

    assert_eq!(buffer.len(), 2);
    assert_eq!(buffer.pop().unwrap().timestamp, 20);
    assert_eq!(buffer.pop().unwrap().timestamp, 30);
    assert!(buffer.pop().is_none());
}

#[test]
fn test_ignore_old_frames() {
    let mut buffer = JitterBuffer::new();

    buffer.push(create_test_frame(20));
    buffer.push(create_test_frame(30));

    // Pop one frame, setting last_popped_timestamp to 20
    assert_eq!(buffer.pop().unwrap().timestamp, 20);

    // Try to push a frame with timestamp <= last_popped
    buffer.push(create_test_frame(15)); // Should be ignored
    buffer.push(create_test_frame(20)); // Should be ignored

    // Buffer should only contain frame with timestamp 30
    assert_eq!(buffer.len(), 1);
    assert_eq!(buffer.pop().unwrap().timestamp, 30);
}

#[test]
fn test_clear() {
    let mut buffer = JitterBuffer::new();

    buffer.push(create_test_frame(10));
    buffer.push(create_test_frame(20));

    assert_eq!(buffer.len(), 2);
    buffer.clear();
    assert_eq!(buffer.len(), 0);
    assert!(buffer.is_empty());
}

#[test]
fn test_pull_frames() {
    let mut buffer = JitterBuffer::new();

    buffer.push(create_test_frame(10));
    buffer.push(create_test_frame(20));
    buffer.push(create_test_frame(30));

    // Pull frames for 40ms (should get 2 frames)
    let frames = buffer.pull_frames(40);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].timestamp, 10);
    assert_eq!(frames[1].timestamp, 20);

    // One frame should remain
    assert_eq!(buffer.len(), 1);
}
