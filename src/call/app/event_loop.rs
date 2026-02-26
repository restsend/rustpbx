use super::app_context::ApplicationContext;
use super::controller::ControllerEvent;
use super::{AppAction, AppEvent, CallApp, CallController, ExitReason};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// The main event loop driving a [`CallApp`].
/// A `CallApp` is primarily reactive: it responds to events dispatched by this loop.
pub struct AppEventLoop {
    app: Box<dyn CallApp>,
    controller: CallController,
    context: ApplicationContext,
    cancel_token: CancellationToken,
}

impl AppEventLoop {
    pub fn new(
        app: Box<dyn CallApp>,
        controller: CallController,
        context: ApplicationContext,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            app,
            controller,
            context,
            cancel_token,
        }
    }

    /// Run the application until it exits or the call ends.
    pub async fn run(mut self) -> anyhow::Result<()> {
        // Initial entry point
        let mut action = self
            .app
            .on_enter(&mut self.controller, &self.context)
            .await?;

        loop {
            // Check cancellation token first
            if self.cancel_token.is_cancelled() {
                self.app.on_exit(ExitReason::Cancelled).await?;
                return Ok(());
            }

            match action {
                AppAction::Continue => {
                    // Wait for next event
                    action = self.handle_next_event().await?;
                }
                AppAction::Exit => {
                    self.app.on_exit(ExitReason::Normal).await?;
                    break;
                }
                AppAction::Hangup { reason } => {
                    // Send hangup command
                    self.controller.hangup(reason.clone()).await?;
                    self.app.on_exit(ExitReason::Hangup).await?;
                    break;
                }
                AppAction::Transfer(_target) => {
                    // Send transfer command
                    // TODO: self.controller.blind_transfer(target).await?;
                    self.app.on_exit(ExitReason::Transferred).await?;
                    break;
                }
                AppAction::Chain(next_app) => {
                    // Exit current app
                    self.app.on_exit(ExitReason::Chained).await?;
                    // Switch to new app
                    self.app = next_app;
                    // Re-enter with new app
                    action = self
                        .app
                        .on_enter(&mut self.controller, &self.context)
                        .await?;
                }
                AppAction::Sleep(duration) => {
                    sleep(duration).await;
                    action = AppAction::Continue;
                }
            }
        }

        Ok(())
    }

    async fn handle_next_event(&mut self) -> anyhow::Result<AppAction> {
        tokio::select! {
            event = self.controller.wait_event() => {
                match event {
                    Some(ControllerEvent::DtmfReceived(digit)) => {
                        self.app.on_dtmf(digit, &mut self.controller, &self.context).await
                    }
                    Some(ControllerEvent::AudioComplete { track_id, .. }) => {
                        self.app.on_audio_complete(track_id, &mut self.controller, &self.context).await
                    }
                    Some(ControllerEvent::RecordingComplete(info)) => {
                        self.app.on_record_complete(info, &mut self.controller, &self.context).await
                    }
                    Some(ControllerEvent::Hangup(reason)) => {
                        self.app.on_exit(ExitReason::RemoteHangup(reason)).await?;
                        // Force exit loop
                        Ok(AppAction::Exit)
                    }
                    Some(ControllerEvent::Custom(name, data)) => {
                        self.app.on_external_event(
                            AppEvent::Custom { name, data },
                            &mut self.controller,
                            &self.context
                        ).await
                    }
                    None => {
                        // Channel closed
                        Ok(AppAction::Exit)
                    }
                }
            }
            _ = self.cancel_token.cancelled() => {
                self.app.on_exit(ExitReason::Cancelled).await?;
                Ok(AppAction::Exit)
            }
        }
    }
}
