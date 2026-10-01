use super::app_context::ApplicationContext;
use super::controller::ControllerEvent;
use super::{AppAction, AppEvent, CallApp, CallController, ExitReason};
use std::future::Future;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// The main event loop driving a [`CallApp`].
/// A `CallApp` is primarily reactive: it responds to events dispatched by this loop.
pub struct AppEventLoop {
    app: Box<dyn CallApp>,
    controller: CallController,
    context: ApplicationContext,
    cancel_token: CancellationToken,
    /// Receives timer IDs fired by `CallController::set_timeout`.
    fired_timer_rx: mpsc::UnboundedReceiver<String>,
    exit_reason: Option<ExitReason>,
}

impl AppEventLoop {
    async fn await_or_cancel<F>(
        cancel_token: &CancellationToken,
        future: F,
    ) -> WaitResult<F::Output>
    where
        F: Future,
    {
        tokio::select! {
            res = future => WaitResult::Completed(res),
            _ = cancel_token.cancelled() => WaitResult::Cancelled,
        }
    }

    pub fn new(
        app: Box<dyn CallApp>,
        controller: CallController,
        context: ApplicationContext,
        cancel_token: CancellationToken,
        fired_timer_rx: mpsc::UnboundedReceiver<String>,
    ) -> Self {
        Self {
            app,
            controller,
            context,
            cancel_token,
            fired_timer_rx,
            exit_reason: None,
        }
    }

    /// Run the application until it exits or the call ends.
    pub async fn run(mut self) -> anyhow::Result<()> {
        // Initial entry point
        let mut action = match Self::await_or_cancel(
            &self.cancel_token,
            self.app.on_enter(&mut self.controller, &self.context),
        )
        .await
        {
            WaitResult::Completed(res) => res?,
            WaitResult::Cancelled => {
                self.app.on_exit(ExitReason::Cancelled).await?;
                return Ok(());
            }
        };

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
                    // The app asked for a plain exit (no transfer/hangup).
                    // on_exit MUST still run: it is where apps flush their
                    // final state — e.g. a tree IVR started via ivr.exec
                    // writes the IvrExecResult (collected digits) there.
                    // Skipping it left those results unwritten and the
                    // ivr.exec hook fell back to an empty default payload.
                    self.app
                        .on_exit(self.exit_reason.take().unwrap_or(ExitReason::Normal))
                        .await?;
                    break;
                }
                AppAction::Hangup { reason, code } => {
                    // Send hangup command
                    self.controller.hangup(reason.clone(), code).await?;
                    self.app.on_exit(ExitReason::Hangup).await?;
                    break;
                }
                AppAction::Transfer(target) => {
                    self.controller.transfer(target).await?;
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
                        match Self::await_or_cancel(&self.cancel_token, self.app.on_dtmf(digit, &mut self.controller, &self.context)).await {
                            WaitResult::Completed(res) => res,
                            WaitResult::Cancelled => {
                                self.exit_reason = Some(ExitReason::Cancelled);
                                Ok(AppAction::Exit)
                            }
                        }
                    }
                    Some(ControllerEvent::AudioComplete { track_id, interrupted }) => {
                        if interrupted {
                            Ok(AppAction::Continue)
                        } else {
                            match Self::await_or_cancel(&self.cancel_token, self.app.on_audio_complete(track_id, &mut self.controller, &self.context)).await {
                                WaitResult::Completed(res) => res,
                                WaitResult::Cancelled => {
                                    self.exit_reason = Some(ExitReason::Cancelled);
                                    Ok(AppAction::Exit)
                                }
                            }
                        }
                    }
                    Some(ControllerEvent::RecordingComplete(info)) => {
                        match Self::await_or_cancel(&self.cancel_token, self.app.on_record_complete(info, &mut self.controller, &self.context)).await {
                            WaitResult::Completed(res) => res,
                            WaitResult::Cancelled => {
                                self.exit_reason = Some(ExitReason::Cancelled);
                                Ok(AppAction::Exit)
                            }
                        }
                    }
                    Some(ControllerEvent::Hangup(reason)) => {
                        self.exit_reason = Some(ExitReason::RemoteHangup(reason));
                        Ok(AppAction::Exit)
                    }
                    Some(ControllerEvent::Timeout(id)) => {
                        match Self::await_or_cancel(&self.cancel_token, self.app.on_timeout(id, &mut self.controller, &self.context)).await {
                            WaitResult::Completed(res) => res,
                            WaitResult::Cancelled => {
                                self.exit_reason = Some(ExitReason::Cancelled);
                                Ok(AppAction::Exit)
                            }
                        }
                    }
                    Some(ControllerEvent::TransferResult(outcome)) => {
                        match Self::await_or_cancel(&self.cancel_token, self.app.on_external_event(
                            AppEvent::TransferResult { outcome },
                            &mut self.controller,
                            &self.context
                        )).await {
                            WaitResult::Completed(res) => res,
                            WaitResult::Cancelled => {
                                self.exit_reason = Some(ExitReason::Cancelled);
                                Ok(AppAction::Exit)
                            }
                        }
                    }
                    Some(ControllerEvent::Custom(name, data)) => {
                        match Self::await_or_cancel(&self.cancel_token, self.app.on_external_event(
                            AppEvent::Custom { name, data },
                            &mut self.controller,
                            &self.context
                        )).await {
                            WaitResult::Completed(res) => res,
                            WaitResult::Cancelled => {
                                self.exit_reason = Some(ExitReason::Cancelled);
                                Ok(AppAction::Exit)
                            }
                        }
                    }
                    None => {
                        self.exit_reason = Some(ExitReason::Normal);
                        Ok(AppAction::Exit)
                    }
                }
            }
            Some(timer_id) = self.fired_timer_rx.recv() => {
                match Self::await_or_cancel(&self.cancel_token, self.app.on_timeout(timer_id, &mut self.controller, &self.context)).await {
                    WaitResult::Completed(res) => res,
                    WaitResult::Cancelled => {
                        self.exit_reason = Some(ExitReason::Cancelled);
                        Ok(AppAction::Exit)
                    }
                }
            }
            _ = self.cancel_token.cancelled() => {
                self.exit_reason = Some(ExitReason::Cancelled);
                Ok(AppAction::Exit)
            }
        }
    }
}
enum WaitResult<T> {
    Completed(T),
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::app::{CallAppType, testing::MockCallStack};
    use std::sync::{Arc, Mutex};

    struct ExitCaptureApp {
        exits: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl CallApp for ExitCaptureApp {
        fn app_type(&self) -> CallAppType {
            CallAppType::Custom
        }

        fn name(&self) -> &str {
            "exit-capture"
        }

        async fn on_enter(
            &mut self,
            _controller: &mut CallController,
            _context: &ApplicationContext,
        ) -> anyhow::Result<AppAction> {
            Ok(AppAction::Continue)
        }

        async fn on_exit(&mut self, reason: ExitReason) -> anyhow::Result<()> {
            self.exits.lock().unwrap().push(reason.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn remote_hangup_exits_once_with_original_reason() {
        let exits = Arc::new(Mutex::new(Vec::new()));
        let stack = MockCallStack::run(
            Box::new(ExitCaptureApp {
                exits: exits.clone(),
            }),
            "caller",
            "callee",
        );

        stack.remote_hangup();
        stack.join().await.expect("event loop should exit cleanly");

        assert_eq!(exits.lock().unwrap().as_slice(), ["remote_hangup(None)"]);
    }
}
