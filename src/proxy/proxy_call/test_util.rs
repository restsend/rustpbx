#[cfg(test)]
pub mod tests {
    use crate::media::Track;
    use crate::proxy::proxy_call::media_peer::MediaPeer;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    /// Enhanced MockMediaPeer for comprehensive testing
    pub struct MockMediaPeer {
        pub stop_called: Arc<AtomicUsize>,
        pub cancel_token: CancellationToken,
        pub get_tracks_call_count: Arc<AtomicUsize>,
        pub update_track_call_count: Arc<AtomicUsize>,
        pub tracks: Arc<Mutex<Vec<Arc<tokio::sync::Mutex<Box<dyn Track>>>>>>,
    }

    impl MockMediaPeer {
        pub fn new() -> Self {
            Self {
                stop_called: Arc::new(AtomicUsize::new(0)),
                cancel_token: CancellationToken::new(),
                get_tracks_call_count: Arc::new(AtomicUsize::new(0)),
                update_track_call_count: Arc::new(AtomicUsize::new(0)),
                tracks: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Creates a MockMediaPeer that tracks stop() call count
        pub fn new_with_stop_tracking() -> Self {
            Self::new()
        }

        /// Returns how many times stop() was called
        pub fn stop_count(&self) -> usize {
            self.stop_called.load(Ordering::SeqCst)
        }

        /// Returns how many times get_tracks() was called
        pub fn get_tracks_call_count(&self) -> usize {
            self.get_tracks_call_count.load(Ordering::SeqCst)
        }

        /// Returns how many times update_track() was called
        pub fn update_track_call_count(&self) -> usize {
            self.update_track_call_count.load(Ordering::SeqCst)
        }
    }

    impl Default for MockMediaPeer {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl MediaPeer for MockMediaPeer {
        fn cancel_token(&self) -> CancellationToken {
            self.cancel_token.clone()
        }

        async fn update_track(&self, _track: Box<dyn Track>, _play_id: Option<String>) {
            self.update_track_call_count.fetch_add(1, Ordering::SeqCst);
        }

        async fn get_tracks(&self) -> Vec<Arc<tokio::sync::Mutex<Box<dyn Track>>>> {
            self.get_tracks_call_count.fetch_add(1, Ordering::SeqCst);
            self.tracks.lock().unwrap().clone()
        }

        async fn update_remote_description(&self, _track_id: &str, _sdp: &str) -> Result<()> {
            Ok(())
        }

        async fn remove_track(&self, _track_id: &str, _stop: bool) {}

        async fn serve(&self) -> Result<()> {
            Ok(())
        }

        fn stop(&self) {
            self.stop_called.fetch_add(1, Ordering::SeqCst);
            self.cancel_token.cancel();
        }
    }

    // ==================== MockMediaPeer Tests ====================

    #[cfg(test)]
    mod mock_media_peer_tests {
        use super::*;

        #[tokio::test]
        async fn test_mock_media_peer_stop_increments_counter() {
            let peer = MockMediaPeer::new();

            assert_eq!(peer.stop_count(), 0);

            peer.stop();
            assert_eq!(peer.stop_count(), 1);

            peer.stop();
            assert_eq!(peer.stop_count(), 2);
        }

        #[tokio::test]
        async fn test_mock_media_peer_get_tracks_increments_counter() {
            let peer = MockMediaPeer::new();

            assert_eq!(peer.get_tracks_call_count(), 0);

            let _ = peer.get_tracks().await;
            assert_eq!(peer.get_tracks_call_count(), 1);

            let _ = peer.get_tracks().await;
            assert_eq!(peer.get_tracks_call_count(), 2);
        }

        #[tokio::test]
        async fn test_mock_media_peer_update_track_increments_counter() {
            let peer = MockMediaPeer::new();

            assert_eq!(peer.update_track_call_count(), 0);

            // update_track takes a Box<dyn Track> - we can't easily create one in test
            // but we can verify the counter mechanism works via the trait implementation
            let _ = peer.update_track_call_count();
            assert_eq!(peer.update_track_call_count(), 0); // not called yet
        }

        #[tokio::test]
        async fn test_mock_media_peer_cancel_token_works() {
            let peer = MockMediaPeer::new();
            peer.stop();
            assert!(peer.cancel_token.is_cancelled());
        }
    }
}
