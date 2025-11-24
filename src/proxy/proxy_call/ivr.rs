type PromptResponseSender = std_mpsc::Sender<Result<PromptPlayback>>;
type InputResponseSender = std_mpsc::Sender<Result<InputEvent>>;

enum IvrRuntimeCommand {
    PlayPrompt {
        _step_id: String,
        prompt: PromptMedia,
        allow_barge_in: bool,
        responder: PromptResponseSender,
    },
    CollectInput {
        _step_id: String,
        input: InputStep,
        _attempt: u32,
        responder: InputResponseSender,
    },
}

struct BlockingIvrRuntime {
    command_tx: mpsc::UnboundedSender<IvrRuntimeCommand>,
}

impl BlockingIvrRuntime {
    fn new(command_tx: mpsc::UnboundedSender<IvrRuntimeCommand>) -> Self {
        Self { command_tx }
    }

    fn send(&self, command: IvrRuntimeCommand) -> Result<()> {
        self.command_tx
            .send(command)
            .map_err(|_| anyhow!("IVR command processor dropped"))
    }

    fn await_response<T>(receiver: std_mpsc::Receiver<Result<T>>) -> Result<T> {
        Ok(receiver
            .recv()
            .map_err(|_| anyhow!("IVR command response channel closed"))??)
    }
}

impl IvrRuntime for BlockingIvrRuntime {
    fn play_prompt(
        &mut self,
        _plan: &IvrPlan,
        step_id: &str,
        prompt: &PromptMedia,
        allow_barge_in: bool,
    ) -> Result<PromptPlayback> {
        let (tx, rx) = std_mpsc::channel();
        self.send(IvrRuntimeCommand::PlayPrompt {
            _step_id: step_id.to_string(),
            prompt: prompt.clone(),
            allow_barge_in,
            responder: tx,
        })?;
        Self::await_response(rx)
    }

    fn collect_input(
        &mut self,
        _plan: &IvrPlan,
        step_id: &str,
        input: &InputStep,
        attempt: u32,
    ) -> Result<InputEvent> {
        let (tx, rx) = std_mpsc::channel();
        self.send(IvrRuntimeCommand::CollectInput {
            _step_id: step_id.to_string(),
            input: input.clone(),
            _attempt: attempt,
            responder: tx,
        })?;
        Self::await_response(rx)
    }
}

struct IvrCommandProcessor<'a> {
    session: &'a mut CallSession,
    events: IvrEventStream,
    prompt_counter: u64,
}

impl<'a> IvrCommandProcessor<'a> {
    fn new(session: &'a mut CallSession, receiver: EventReceiver) -> Self {
        Self {
            session,
            events: IvrEventStream::new(receiver),
            prompt_counter: 0,
        }
    }

    async fn process(&mut self, command: IvrRuntimeCommand) -> Result<()> {
        match command {
            IvrRuntimeCommand::PlayPrompt {
                prompt,
                allow_barge_in,
                responder,
                ..
            } => {
                let result = self.handle_play_prompt(prompt, allow_barge_in).await;
                let _ = responder.send(result);
            }
            IvrRuntimeCommand::CollectInput {
                input, responder, ..
            } => {
                let result = self.handle_collect_input(input).await;
                let _ = responder.send(result);
            }
        }
        Ok(())
    }

    async fn ensure_answered(&mut self) -> Result<()> {
        if !self.session.is_answered() {
            self.session.accept_call(None, None).await?;
        }
        Ok(())
    }

    fn next_play_id(&mut self) -> String {
        self.prompt_counter += 1;
        format!("ivr-prompt-{}", self.prompt_counter)
    }

    async fn handle_play_prompt(
        &mut self,
        prompt: PromptMedia,
        allow_barge_in: bool,
    ) -> Result<PromptPlayback> {
        self.ensure_answered().await?;
        let repeat = prompt.loop_count.unwrap_or(1).max(1) as usize;
        let mut last = PromptPlayback::Completed;
        for _ in 0..repeat {
            let play_id = self.next_play_id();
            self.start_prompt_track(&prompt, &play_id).await?;
            let result = self
                .events
                .wait_for_prompt(play_id.clone(), allow_barge_in)
                .await;
            self.stop_prompt_track().await;
            last = result?;
            if matches!(last, PromptPlayback::BargeIn) {
                break;
            }
        }
        Ok(last)
    }

    async fn start_prompt_track(&mut self, prompt: &PromptMedia, play_id: &str) -> Result<()> {
        match &prompt.source {
            PromptSource::File { file } => {
                let track = FileTrack::new(CallSession::IVR_PROMPT_TRACK_ID.to_string())
                    .with_path(file.path.clone())
                    .with_ssrc(rand::random::<u32>())
                    .with_cancel_token(self.session.media_stream.cancel_token.child_token());
                self.session
                    .media_stream
                    .update_track(Box::new(track), Some(play_id.to_string()))
                    .await;
                Ok(())
            }
            PromptSource::Tts { .. } => Err(anyhow!("TTS prompts are not supported yet")),
            PromptSource::Url { .. } => Err(anyhow!("URL prompts are not supported yet")),
        }
    }

    async fn stop_prompt_track(&mut self) {
        self.session
            .media_stream
            .remove_track(&CallSession::IVR_PROMPT_TRACK_ID.to_string(), false)
            .await;
    }

    async fn handle_collect_input(&mut self, input: InputStep) -> Result<InputEvent> {
        self.ensure_answered().await?;
        let mut digits = String::new();
        self.events
            .drain_pending(&mut digits, input.max_digits.unwrap_or(usize::MAX));
        if let Some(max) = input.max_digits {
            if digits.len() >= max {
                return Ok(InputEvent::Digits(digits));
            }
        }

        loop {
            let timeout_ms = input.timeout_ms.unwrap_or(5_000);
            match self
                .events
                .next_digit(Some(Duration::from_millis(timeout_ms)))
                .await?
            {
                DigitWaitResult::Digit(digit) => {
                    digits.push_str(&digit);
                    if let Some(max) = input.max_digits {
                        if digits.len() >= max {
                            return Ok(InputEvent::Digits(digits));
                        }
                    }
                }
                DigitWaitResult::Timeout => {
                    if digits.is_empty() {
                        return Ok(InputEvent::Timeout);
                    }
                    return Ok(InputEvent::Digits(digits));
                }
                DigitWaitResult::Cancelled => return Ok(InputEvent::Cancel),
            }
        }
    }
}

struct IvrEventStream {
    receiver: EventReceiver,
    pending_digits: VecDeque<String>,
}

impl IvrEventStream {
    fn new(receiver: EventReceiver) -> Self {
        Self {
            receiver,
            pending_digits: VecDeque::new(),
        }
    }

    fn drain_pending(&mut self, buffer: &mut String, max: usize) {
        while buffer.len() < max {
            if let Some(digit) = self.pending_digits.pop_front() {
                buffer.push_str(&digit);
            } else {
                break;
            }
        }
    }

    async fn wait_for_prompt(
        &mut self,
        play_id: String,
        allow_barge_in: bool,
    ) -> Result<PromptPlayback> {
        loop {
            match self.receiver.recv().await {
                Ok(SessionEvent::TrackEnd {
                    play_id: Some(id), ..
                }) if id == play_id => {
                    return Ok(PromptPlayback::Completed);
                }
                Ok(SessionEvent::Dtmf { digit, .. }) => {
                    self.pending_digits.push_back(digit.clone());
                    if allow_barge_in {
                        return Ok(PromptPlayback::BargeIn);
                    }
                }
                Ok(SessionEvent::Hangup { .. }) => {
                    return Err(anyhow!("call ended during IVR prompt"));
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(anyhow!("event channel closed"));
                }
                Err(broadcast::error::RecvError::Lagged(_)) | Ok(_) => {
                    continue;
                }
            }
        }
    }

    async fn next_digit(&mut self, timeout: Option<Duration>) -> Result<DigitWaitResult> {
        if let Some(digit) = self.pending_digits.pop_front() {
            return Ok(DigitWaitResult::Digit(digit));
        }

        if let Some(duration) = timeout {
            match tokio::time::timeout(duration, self.receiver.recv()).await {
                Ok(result) => self.handle_digit_event(result),
                Err(_) => Ok(DigitWaitResult::Timeout),
            }
        } else {
            let event = self.receiver.recv().await;
            self.handle_digit_event(event)
        }
    }

    fn handle_digit_event(
        &mut self,
        event: Result<SessionEvent, broadcast::error::RecvError>,
    ) -> Result<DigitWaitResult> {
        match event {
            Ok(SessionEvent::Dtmf { digit, .. }) => Ok(DigitWaitResult::Digit(digit)),
            Ok(SessionEvent::Hangup { .. }) => Ok(DigitWaitResult::Cancelled),
            Ok(_) => Ok(DigitWaitResult::Timeout),
            Err(broadcast::error::RecvError::Lagged(_)) => Ok(DigitWaitResult::Timeout),
            Err(broadcast::error::RecvError::Closed) => Ok(DigitWaitResult::Cancelled),
        }
    }
}

enum DigitWaitResult {
    Digit(String),
    Timeout,
    Cancelled,
}

impl ProxyCall {
    async fn transfer_to_ivr(&self, session: &mut CallSession, reference: &str) -> Result<()> {
        if reference.trim().is_empty() {
            return Err(anyhow!("ivr reference cannot be empty"));
        }
        let config = self
            .server
            .data_context
            .resolve_ivr_config(reference)
            .await?
            .ok_or_else(|| anyhow!("ivr reference '{}' not found", reference))?;
        let dialplan_config = config.to_dialplan_config()?;
        self.run_ivr(session, &dialplan_config, Some(reference)).await
    }

    async fn run_ivr(
        &self,
        session: &mut CallSession,
        config: &DialplanIvrConfig,
        source_reference: Option<&str>,
    ) -> Result<()> {
        session.stop_queue_hold().await;
        session.note_ivr_reference(
            source_reference.map(|value| value.to_string()).or_else(|| config.plan_id.clone()),
            config.plan_id.clone(),
        );
        let plan = if let Some(plan) = config.plan.clone() {
            plan
        } else if let Some(plan_id) = config.plan_id.as_deref() {
            let fallback = self
                .server
                .data_context
                .resolve_ivr_config(plan_id)
                .await?
                .ok_or_else(|| anyhow!("ivr plan '{}' not found", plan_id))?;
            fallback
                .to_dialplan_config()?
                .plan
                .ok_or_else(|| anyhow!("ivr plan '{}' missing inline data", plan_id))?
        } else {
            return Err(anyhow!("IVR config requires inline plan data"));
        };
        let plan = Arc::new(plan);

        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let availability_override = config.availability_override;
        let executor_plan = plan.clone();
        let executor_handle = tokio::task::spawn_blocking(move || {
            let mut runtime = BlockingIvrRuntime::new(cmd_tx);
            let mut executor = IvrExecutor::new(executor_plan.as_ref(), &mut runtime)
                .with_availability_override(availability_override);
            executor.run()
        });

        {
            let event_rx = session.event_sender.subscribe();
            let mut processor = IvrCommandProcessor::new(session, event_rx);
            while let Some(command) = cmd_rx.recv().await {
                if let Err(err) = processor.process(command).await {
                    warn!(
                        session_id = %self.session_id,
                        error = %err,
                        "failed to process IVR command"
                    );
                    return Err(err);
                }
            }
        }

        let exit = executor_handle
            .await
            .map_err(|e| anyhow!("IVR executor panic: {e}"))??;
        let clone = exit.clone();
        let result = self.handle_ivr_exit(session, exit).await;
        session.note_ivr_exit(&clone);
        result
    }

    async fn handle_ivr_exit(&self, session: &mut CallSession, exit: IvrExit) -> Result<()> {
        match exit {
            IvrExit::Completed => Ok(()),
            IvrExit::Transfer(action) => self.handle_ivr_transfer(session, action).await,
            IvrExit::Hangup(hangup) => {
                let status = hangup.code.map(StatusCode::from).unwrap_or(StatusCode::OK);
                session.set_error(status, hangup.reason.clone(), None);
                Ok(())
            }
            IvrExit::Queue(_) => Err(anyhow!("Queue actions from IVR are not supported yet")),
            IvrExit::Webhook(_) => Err(anyhow!("Webhook actions from IVR are not supported yet")),
            IvrExit::Playback(_) => Err(anyhow!("Playback-only exits are not supported yet")),
        }
    }

    async fn handle_ivr_transfer(
        &self,
        session: &mut CallSession,
        action: ResolvedTransferAction,
    ) -> Result<()> {
        let uri = Uri::try_from(action.target.as_str())?;
        let mut location = Location::default();
        location.aor = uri.clone();
        location.contact_raw = Some(uri.to_string());
        if !action.headers.is_empty() {
            let mut headers = Vec::new();
            for (name, value) in action.headers {
                headers.push(rsip::Header::Other(name, value));
            }
            location.headers = Some(headers);
        }
        if let Some(caller_id) = action.caller_id {
            session.routed_caller = Some(caller_id);
        }
        self.try_single_target(session, &location)
            .await
            .map_err(|(code, reason)| {
                session.set_error(code.clone(), reason.clone(), None);
                anyhow!(
                    "IVR transfer failed: {}",
                    reason.unwrap_or_else(|| code.to_string())
                )
            })
    }
}
