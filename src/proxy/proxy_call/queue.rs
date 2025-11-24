type QueueFallbackResult<'a> = (
    &'a mut CallSession,
    &'a QueueFallbackAction,
    &'a DialplanFlow,
    usize,
    bool,
);

impl ProxyCall {
    async fn transfer_to_queue(&self, session: &mut CallSession, reference: &str) -> Result<()> {
        let queue_plan = self.load_queue_plan(reference).await?;
        let strategy = queue_plan
            .dial_strategy()
            .cloned()
            .ok_or_else(|| anyhow!("queue '{}' has no dial targets", reference))?;
        let next_flow = DialplanFlow::Targets(strategy);
        session.set_queue_name(Some(reference.to_string()));
        self.execute_queue_plan(session, &queue_plan, &next_flow, 0, false)
            .await
    }

    async fn load_queue_plan(&self, reference: &str) -> Result<QueuePlan> {
        if reference.trim().is_empty() {
            return Err(anyhow!("queue reference cannot be empty"));
        }
        let label = reference.trim().to_string();
        if let Some(config) = self
            .server
            .data_context
            .resolve_queue_config(reference)
            .await?
        {
            let mut plan = config.to_queue_plan()?;
            if plan.label.is_none() {
                plan.label = Some(label.clone());
            }
            return Ok(plan);
        }
        if let Some(config) = self.server.data_context.load_queue_file(reference).await? {
            let mut plan = config.to_queue_plan()?;
            if plan.label.is_none() {
                plan.label = Some(label);
            }
            return Ok(plan);
        }
        Err(anyhow!("queue reference '{}' not found", reference))
    }

    async fn execute_queue_plan(
        &self,
        session: &mut CallSession,
        plan: &QueuePlan,
        next: &DialplanFlow,
        depth: usize,
        propagate_failure: bool,
    ) -> Result<()> {
        if depth >= MAX_QUEUE_CHAIN_DEPTH {
            warn!(
                session_id = %self.session_id,
                depth,
                "queue chain exceeded maximum depth"
            );
            return self.handle_failure(session).await;
        }
        if let Some(name) = plan.label.clone() {
            session.set_queue_name(Some(name));
        }
        let previous_passthrough = session.queue_ringback_passthrough();
        let enable_passthrough = plan.accept_immediately && plan.passthrough_ringback();
        session.set_queue_ringback_passthrough(enable_passthrough);

        let result = async {
            if plan.accept_immediately {
                session.accept_call(None, None).await?;
            } else if !session.early_media_sent {
                session.start_ringing(String::new(), self).await;
            }

            if let Some(hold) = plan.hold.clone() {
                if let Err(e) = session.start_queue_hold(hold).await {
                    warn!(session_id = %self.session_id, error = %e, "Failed to start queue hold track");
                }
            }

            let dial_future = self.execute_flow(session, next);
            let dial_result = if let Some(timeout_duration) = plan.ring_timeout {
                match timeout(timeout_duration, dial_future).await {
                    Ok(result) => result,
                    Err(_) => {
                        warn!(
                            session_id = %self.session_id,
                            timeout = ?timeout_duration,
                            "queue dial attempt timed out"
                        );
                        Err(anyhow!("queue dial timed out after {:?}", timeout_duration))
                    }
                }
            } else {
                dial_future.await
            };

            session.stop_queue_hold().await;

            match dial_result {
                Ok(_) => Ok(()),
                Err(_) => {
                    if let Some(fallback) = &plan.fallback {
                        self.execute_queue_fallback((session, fallback, next, depth, propagate_failure))
                            .await
                    } else if propagate_failure {
                        self.handle_failure(session).await
                    } else {
                        Err(anyhow!("queue plan execution failed"))
                    }
                }
            }
        }
        .await;

        session.set_queue_ringback_passthrough(previous_passthrough);
        session.set_queue_name(None);
        result
    }

    fn execute_queue_fallback<'a>(
        &'a self,
        args: QueueFallbackResult<'a>,
    ) -> BoxFuture<'a, Result<()>> {
        let (session, fallback, next, depth, propagate_failure) = args;
        async move {
            match fallback {
                QueueFallbackAction::Failure(action) => {
                    self.execute_failure_action(session, action).await
                }
                QueueFallbackAction::Redirect { target } => {
                    let mut location = Location::default();
                    location.aor = target.clone();
                    match self.try_single_target(session, &location).await {
                        Ok(_) => Ok(()),
                        Err(_) => {
                            if propagate_failure {
                                self.handle_failure(session).await
                            } else {
                                Err(anyhow!("queue fallback redirect target failed to answer"))
                            }
                        }
                    }
                }
                QueueFallbackAction::Queue { name } => {
                    let config = match self.server.data_context.resolve_queue_config(name).await {
                        Ok(Some(cfg)) => cfg,
                        Ok(None) => {
                            warn!(
                                session_id = %self.session_id,
                                queue = name,
                                "queue fallback references unknown queue"
                            );
                            return self.handle_failure(session).await;
                        }
                        Err(err) => {
                            warn!(
                                session_id = %self.session_id,
                                queue = name,
                                error = %err,
                                "failed to load queue fallback config"
                            );
                            return self.handle_failure(session).await;
                        }
                    };
                    match config.to_queue_plan() {
                        Ok(mut plan) => {
                            if plan.label.is_none() {
                                plan.label = Some(name.clone());
                            }
                            self.execute_queue_plan(
                                session,
                                &plan,
                                next,
                                depth + 1,
                                propagate_failure,
                            )
                            .await
                        }
                        Err(err) => {
                            warn!(
                                session_id = %self.session_id,
                                queue = name,
                                error = %err,
                                "failed to build fallback queue plan"
                            );
                            if propagate_failure {
                                self.handle_failure(session).await
                            } else {
                                Err(anyhow!("queue fallback plan failed"))
                            }
                        }
                    }
                }
            }
        }
        .boxed()
    }
}
