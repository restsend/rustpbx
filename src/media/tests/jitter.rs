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
    assert!(buffer.push(create_test_frame(30)));
    assert!(buffer.push(create_test_frame(10)));
    assert!(buffer.push(create_test_frame(20)));

    // Should pop in timestamp order (oldest first)
    assert_eq!(buffer.pop().unwrap().timestamp, 10);
    assert_eq!(buffer.pop().unwrap().timestamp, 20);
    assert_eq!(buffer.pop().unwrap().timestamp, 30);
    assert!(buffer.pop().is_none());
}

#[test]
fn test_max_size() {
    let mut buffer = JitterBuffer::with_max_size(2);

    assert!(buffer.push(create_test_frame(10)));
    assert!(buffer.push(create_test_frame(20)));
    assert!(buffer.push(create_test_frame(30))); // This should replace the oldest frame (10)

    assert_eq!(buffer.len(), 2);
    assert_eq!(buffer.pop().unwrap().timestamp, 20);
    assert_eq!(buffer.pop().unwrap().timestamp, 30);
    assert!(buffer.pop().is_none());
}

#[test]
fn test_ignore_old_frames() {
    let mut buffer = JitterBuffer::new();

    assert!(buffer.push(create_test_frame(20)));
    assert!(buffer.push(create_test_frame(30)));

    // Pop one frame, setting last_popped_timestamp to 20
    assert_eq!(buffer.pop().unwrap().timestamp, 20);

    // Try to push a frame with timestamp <= last_popped
    assert!(!buffer.push(create_test_frame(15))); // Should be rejected
    assert!(!buffer.push(create_test_frame(20))); // Should be rejected

    // Buffer should only contain frame with timestamp 30
    assert_eq!(buffer.len(), 1);
    assert_eq!(buffer.pop().unwrap().timestamp, 30);
}

#[test]
fn test_clear() {
    let mut buffer = JitterBuffer::new();

    assert!(buffer.push(create_test_frame(10)));
    assert!(buffer.push(create_test_frame(20)));

    assert_eq!(buffer.len(), 2);
    buffer.clear();
    assert_eq!(buffer.len(), 0);
    assert!(buffer.is_empty());
}

#[test]
fn test_pull_frames() {
    let mut buffer = JitterBuffer::new();

    assert!(buffer.push(create_test_frame(10)));
    assert!(buffer.push(create_test_frame(20)));
    assert!(buffer.push(create_test_frame(30)));

    // Pull frames for 40ms (should get 2 frames)
    let frames = buffer.pull_frames(40);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].timestamp, 10);
    assert_eq!(frames[1].timestamp, 20);

    // One frame should remain
    assert_eq!(buffer.len(), 1);
}

#[test]
fn test_statistics() {
    let mut buffer = JitterBuffer::new();

    // Push some frames
    assert!(buffer.push(create_test_frame(10)));
    assert!(buffer.push(create_test_frame(20)));

    // Pop one frame to set last_popped_timestamp
    assert_eq!(buffer.pop().unwrap().timestamp, 10);

    // Now try to push a frame that's older than last_popped
    assert!(!buffer.push(create_test_frame(5))); // Should be rejected as too old

    let stats = buffer.stats();
    assert_eq!(stats.buffer_size, 1); // Only frame with timestamp 20 remains
    assert_eq!(stats.total_received, 3);
    assert_eq!(stats.total_late, 1);
    assert_eq!(stats.total_dropped, 0);
}

#[test]
fn test_duplicate_timestamps() {
    let mut buffer = JitterBuffer::new();

    // Push frame with timestamp 10
    assert!(buffer.push(create_test_frame(10)));
    assert_eq!(buffer.len(), 1);

    // Push another frame with same timestamp - should replace
    assert!(buffer.push(create_test_frame(10)));
    assert_eq!(buffer.len(), 1);

    // Should still have only one frame
    let frame = buffer.pop().unwrap();
    assert_eq!(frame.timestamp, 10);
    assert!(buffer.pop().is_none());
}

#[test]
fn test_pull_frames_with_duration() {
    let mut buffer = JitterBuffer::new();

    assert!(buffer.push(create_test_frame(10)));
    assert!(buffer.push(create_test_frame(20)));
    assert!(buffer.push(create_test_frame(30)));
    assert!(buffer.push(create_test_frame(40)));

    // Pull frames for 60ms with 30ms frame duration (should get 2 frames)
    let frames = buffer.pull_frames_with_duration(60, 30);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].timestamp, 10);
    assert_eq!(frames[1].timestamp, 20);

    // Two frames should remain
    assert_eq!(buffer.len(), 2);
}

#[test]
fn test_buffer_overflow_with_stats() {
    let mut buffer = JitterBuffer::with_max_size(2);

    // Fill buffer to capacity
    assert!(buffer.push(create_test_frame(10)));
    assert!(buffer.push(create_test_frame(20)));
    assert_eq!(buffer.len(), 2);

    // Push new frame - should drop oldest
    assert!(buffer.push(create_test_frame(30)));
    assert_eq!(buffer.len(), 2);

    let stats = buffer.stats();
    assert_eq!(stats.total_received, 3);
    assert_eq!(stats.total_dropped, 1);
    assert_eq!(stats.total_late, 0);

    // Verify remaining frames are the newer ones
    assert_eq!(buffer.pop().unwrap().timestamp, 20);
    assert_eq!(buffer.pop().unwrap().timestamp, 30);
}
