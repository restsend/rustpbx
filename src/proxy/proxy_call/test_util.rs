#[cfg(test)]
pub mod tests {
    use crate::media::Track;
    use crate::proxy::proxy_call::media_peer::MediaPeer;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    pub struct MockMediaPeer {
        pub stop_called: Arc<std::sync::atomic::AtomicBool>,
        pub cancel_token: CancellationToken,
    }

    impl MockMediaPeer {
        pub fn new() -> Self {
            Self {
                stop_called: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                cancel_token: CancellationToken::new(),
            }
        }
    }

    #[async_trait]
    impl MediaPeer for MockMediaPeer {
        fn cancel_token(&self) -> CancellationToken {
            self.cancel_token.clone()
        }
        async fn update_track(&self, _track: Box<dyn Track>, _play_id: Option<String>) {}
        async fn get_tracks(&self) -> Vec<Arc<tokio::sync::Mutex<Box<dyn Track>>>> {
            Vec::new()
        }
        async fn update_remote_description(&self, _track_id: &str, _sdp: &str) -> Result<()> {
            Ok(())
        }
        async fn suppress_forwarding(&self, _track_id: &str) {}
        async fn resume_forwarding(&self, _track_id: &str) {}
        fn is_suppressed(&self, _track_id: &str) -> bool {
            false
        }
        async fn remove_track(&self, _track_id: &str, _stop: bool) {}
        async fn serve(&self) -> Result<()> {
            Ok(())
        }
        fn stop(&self) {
            self.stop_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.cancel_token.cancel();
        }
    }
}
