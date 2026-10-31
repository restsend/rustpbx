use super::prelude::*;
use super::session::{merge_ivr_exec_app_params, SipSession};
use crate::proxy::proxy_call::sip_session::util::other_header_ci;
use crate::call::app::queue::QueueSignal;


// ===================================================================
// App lifecycle for SipSession (app launch/return/exit/recovery,
// ivr_exec command handling, queue app start, queue recovery dial).
// Moved from sip_session/session.rs.
// ===================================================================

impl SipSession {
    /// Start (or restart if already running) an application.
    pub(crate) async fn ensure_app_running(
        &self,
        kind: &str,
        params: Option<serde_json::Value>,
        label: &str,
    ) -> Result<()> {
        self.ensure_app_running_with(kind, params, true, label, None)
            .await
    }

    pub(crate) async fn ensure_app_running_with_route_context(
        &self,
        kind: &str,
        params: Option<serde_json::Value>,
        auto_answer: bool,
        label: &str,
        route_context: crate::call::app::AppRouteContext,
    ) -> Result<()> {
        self.ensure_app_running_with(kind, params, auto_answer, label, Some(route_context))
            .await
    }

    /// Like [`Self::ensure_app_running`] but with an explicit `auto_answer`.
    ///
    /// Shared by `ensure_app_running` and `start_queue_app`: the app runtime
    /// keeps the previously-started app registered (e.g. an IVR that handed
    /// control over via `AppAction::Transfer`) until an explicit `stop_app`,
    /// so `start_app` returns [`AppRuntimeError::AlreadyRunning`]. All app
    /// transitions recover by stopping the stale app and restarting instead of
    /// failing into dead air.
    pub(crate) async fn ensure_app_running_with(
        &self,
        kind: &str,
        params: Option<serde_json::Value>,
        auto_answer: bool,
        label: &str,
        route_context: Option<crate::call::app::AppRouteContext>,
    ) -> Result<()> {
        use crate::call::runtime::AppRuntimeError;
        let result = if let Some(context) = route_context.clone() {
            self.app_runtime
                .start_app_with_route_context(kind, params.clone(), auto_answer, context)
                .await
        } else {
            self.app_runtime
                .start_app(kind, params.clone(), auto_answer)
                .await
        };
        match result {
            Ok(()) => {
                // App now drives the session — suppress the RTP watchdog unless
                // a real callee is already bridged.
                self.sync_rtp_timeout_pause();
                Ok(())
            }
            Err(AppRuntimeError::AlreadyRunning(_)) => {
                warn!(session_id = %self.id, app = %label, "runtime still marked running, restarting app");
                match self
                    .app_runtime
                    .stop_app(Some(format!("restart {}", label)))
                    .await
                {
                    Ok(()) | Err(AppRuntimeError::NotRunning) => {}
                    Err(stop_err) => {
                        warn!(session_id = %self.id, error = ?stop_err, "Failed to stop existing {} app", label)
                    }
                }
                let restarted = if let Some(context) = route_context {
                    self.app_runtime
                        .start_app_with_route_context(kind, params, auto_answer, context)
                        .await
                } else {
                    self.app_runtime.start_app(kind, params, auto_answer).await
                };
                restarted
                    .map(|()| self.sync_rtp_timeout_pause())
                    .map_err(|e| anyhow!("Failed to restart {}: {:?}", label, e))
            }
            Err(e) => Err(anyhow!("Failed to start {}: {:?}", label, e)),
        }
    }

    pub(super) async fn bind_suspended_ivr_exec_to_current_app(&self) {
        if let Some(invocation) = self.app_runtime.current_app_invocation().await {
            crate::proxy::proxy_call::ivr_exec_hook::bind_ivr_exec(
                &self.extensions,
                invocation.app_execution_id,
            );
        }
    }

    pub(crate) async fn complete_suspended_ivr_exec(&mut self, reason: &str) {
        if crate::proxy::proxy_call::ivr_exec_hook::fail_suspended_ivr_exec(
            &self.extensions,
            reason,
        ) {
            self.handle_app_exited(None).await;
        }
    }

    pub(super) async fn fail_suspended_ivr_handoff(&mut self, reason: &str, detail: String) {
        if self.meta.ivr_flow_suspended {
            self.meta.ivr_flow_suspended = false;
            self.emit_suspended_flow_session_end(
                crate::call::app::ivr::provider::SessionEndReason {
                    reason: crate::call::app::ivr::provider::SessionEndTag::Error,
                    detail: Some(detail),
                },
            )
            .await;
        }
        self.complete_suspended_ivr_exec(reason).await;
    }

    /// True while a queue app is driving this session — used to gate
    /// queue-specific extension bookkeeping.
    pub(crate) fn in_queue_context(&self) -> bool {
        self.app_runtime
            .current_app()
            .as_deref()
            .is_some_and(|app| app == "queue")
            || crate::proxy::proxy_call::call_meta::has_queue_name(&self.meta)
    }

    pub(crate) async fn fail_ivr_exec_start(&mut self, reason: String) {
        {
            let mut extensions = self.extensions.write();
            if extensions
                .get::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecState>()
                .is_none()
            {
                return;
            }
            extensions.insert(crate::proxy::proxy_call::ivr_exec_hook::IvrExecResult {
                app_execution_id: None,
                status: "failed".to_string(),
                reason,
                routing_target: None,
                collected: Default::default(),
                trace: Vec::new(),
                duration_ms: 0,
                completion_time: chrono::Utc::now().to_rfc3339(),
            });
        }
        let _ = self.handle_app_exited(None).await;
        let held_leg = {
            let mut extensions = self.extensions.write();
            let held_leg = extensions
                .get::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecState>()
                .and_then(|state| state.held_leg.clone());
            extensions.remove::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecState>();
            extensions.remove::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecResult>();
            extensions.remove::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecRouteHints>();
            held_leg
        };
        if let Some(leg_id) = held_leg
            && let Err(error) = self.handle_unhold(leg_id.clone()).await
        {
            warn!(session_id = %self.id, leg = %leg_id, %error,
                "Failed to release IVR exec hold after start failure");
        }
    }

    /// Handle `ivr.exec` — bundled IVR execution.
    ///
    /// 1. Writes [`IvrExecState`] to session extensions
    /// 2. Holds callee + plays hold music (reuses the propagate-hold path)
    /// 3. Enqueues `StartApp` via the command channel
    pub(crate) async fn handle_ivr_exec_command(
        &mut self,
        params: Option<serde_json::Value>,
        dialog_leg: Option<LegId>,
        tx_handle: &TransactionHandle,
    ) -> Result<()> {
        let session_id = self.context.session_id.clone();
        let p = params.as_ref();
        let request_id = p
            .and_then(|p| p.get("request_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut app_name = p
            .and_then(|p| p.get("app"))
            .and_then(|v| v.as_str())
            .unwrap_or("ivr")
            .to_string();
        let ivr_params = p.and_then(|p| p.get("ivr_params")).cloned();
        let route_point = p
            .and_then(|p| p.get("route_point"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                ivr_params
                    .as_ref()
                    .and_then(|v| v.get("route_point"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
        let ivr_name = p
            .and_then(|p| p.get("ivr_name"))
            .and_then(|v| v.as_str())
            .map(|value| value.to_string());
        let route_point = route_point
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let ivr_name = ivr_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if route_point.is_some() == ivr_name.is_some() {
            warn!(
                session_id = %self.id,
                "ivr.exec requires exactly one of route_point or ivr_name"
            );
            tx_handle
                .respond(rsipstack::sip::StatusCode::BadRequest, None, None)
                .await
                .ok();
            return Ok(());
        }
        if self
            .extensions
            .read()
            .get::<crate::proxy::proxy_call::ivr_exec_hook::IvrExecState>()
            .is_some()
        {
            tx_handle
                .respond(rsipstack::sip::StatusCode::BusyHere, None, None)
                .await
                .ok();
            return Ok(());
        }
        let mut base_params = None;
        let mut route_context = None;
        let mut route_hints = None;
        if let Some(route_point) = route_point {
            let caller = self
                .context
                .dialplan
                .caller
                .clone()
                .ok_or_else(|| anyhow!("ivr.exec route point has no caller identity"))?;
            let current_headers = self
                .app_runtime
                .current_app_invocation()
                .await
                .map(|context| context.sip_headers)
                .or_else(|| {
                    self.app_runtime
                        .app_context()
                        .map(|context| context.call_info.sip_headers.clone())
                })
                .unwrap_or_default();
            let carry_headers = current_headers
                .iter()
                .map(|(name, value)| rsipstack::sip::Header::Other(name.clone(), value.clone()))
                .collect::<Vec<_>>();
            let route_result = match self
                .resolve_route_point(
                    route_point,
                    &caller,
                    (!carry_headers.is_empty()).then_some(carry_headers),
                )
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    warn!(session_id = %self.id, route_point, %error,
                        "ivr.exec route point is invalid");
                    tx_handle
                        .respond(rsipstack::sip::StatusCode::BadRequest, None, None)
                        .await
                        .ok();
                    return Ok(());
                }
            };
            match route_result {
                Some(crate::config::RouteResult::Application {
                    option,
                    app_name: routed_app,
                    app_params,
                    hints,
                    ..
                }) => {
                    app_name = routed_app;
                    base_params = app_params;
                    let mut inherited_headers = current_headers.clone();
                    // flowname selects the routed application flow and is
                    // therefore authoritative only when produced by this
                    // route. Preserve ordinary call-context headers.
                    inherited_headers.retain(|name, _| !name.eq_ignore_ascii_case("flowname"));
                    route_context = Some(crate::call::app::AppRouteContext {
                        callee: route_point.to_string(),
                        sip_headers: crate::call::app::merge_sip_headers(
                            &inherited_headers,
                            option.headers.as_deref().unwrap_or_default(),
                        ),
                        variables: Default::default(),
                    });
                    route_hints = hints;
                }
                Some(crate::config::RouteResult::Abort(status, _)) => {
                    tx_handle.respond(status, None, None).await.ok();
                    return Ok(());
                }
                _ => {
                    warn!(session_id = %self.id, route_point,
                        "ivr.exec route point did not resolve to an application");
                    tx_handle
                        .respond(rsipstack::sip::StatusCode::NotFound, None, None)
                        .await
                        .ok();
                    return Ok(());
                }
            }
        } else if let Some(ivr_name) = ivr_name {
            let file = self.server.data_context.resolve_ivr_file(ivr_name).await;
            base_params = Some(serde_json::json!({ "file": file }));
        }
        let app_params = merge_ivr_exec_app_params(base_params, ivr_params, route_point);
        let hold_agent = p
            .and_then(|p| p.get("hold_agent"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let webhook_url = p
            .and_then(|p| p.get("webhook_url"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let metadata = p
            .and_then(|p| p.get("metadata"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let override_music = p
            .and_then(|p| p.get("music"))
            .and_then(Self::parse_info_media_source);

        // 1. Write IvrExecState so the post-exit hook can reconstruct the result.
        // The initiating leg is whoever sent the INFO on (the agent's dynamic
        // leg in the multi-leg model — NOT necessarily the legacy "callee").
        // Falling back to "callee" keeps the single-callee behaviour intact.
        let caller_initiated = dialog_leg
            .as_ref()
            .map(|leg| leg.as_str() == "caller")
            .unwrap_or(true);
        let initiator_leg = dialog_leg
            .filter(|leg| leg.as_str() != "caller")
            .unwrap_or_else(|| LegId::from("callee"));
        let held_leg_id = if caller_initiated {
            self.legs
                .iter()
                .find(|(id, leg)| id.as_str() != "caller" && leg.state == LegState::Connected)
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| LegId::from("callee"))
        } else {
            initiator_leg.clone()
        };
        {
            let mut ext = self.extensions.write();
            ext.insert(crate::proxy::proxy_call::ivr_exec_hook::IvrExecState {
                app_execution_id: None,
                request_id: request_id.clone(),
                held_leg: if hold_agent {
                    Some(held_leg_id.clone())
                } else {
                    None
                },
                initiator_leg: initiator_leg.clone(),
                webhook_url,
                app_name: app_name.clone(),
                metadata,
            });
            if let Some(hints) = route_hints {
                ext.insert(crate::proxy::proxy_call::ivr_exec_hook::IvrExecRouteHints::new(hints));
            }
        }

        // 2. Hold the agent leg + play music (use override_music if provided,
        // else default). The app must not start unless hold negotiation succeeds.
        if hold_agent {
            if let Err(e) = self.handle_hold(held_leg_id.clone(), override_music).await {
                warn!(session_id = %self.id,
                    session_id = %self.context.session_id,
                    leg = %held_leg_id,
                    error = %e,
                    "ivr.exec: hold of initiator leg failed"
                );
                self.fail_ivr_exec_start("hold_failed".to_string()).await;
                tx_handle
                    .respond(rsipstack::sip::StatusCode::ServerInternalError, None, None)
                    .await
                    .ok();
                return Ok(());
            }
        }

        // 3. Start the app on the caller leg.
        let cmd = match route_context {
            Some(route_context) => CallCommand::StartAppWithRouteContext {
                app_name,
                params: Some(app_params),
                auto_answer: false,
                route_context,
            },
            None => CallCommand::StartApp {
                app_name,
                params: Some(app_params),
                auto_answer: false,
            },
        };
        let enqueued = self
            .cmd_tx
            .as_ref()
            .is_some_and(|tx| tx.try_send(cmd).is_ok());
        if !enqueued {
            warn!(session_id = %self.id, "ivr.exec command could not be enqueued");
            self.fail_ivr_exec_start("app_enqueue_failed".to_string())
                .await;
            tx_handle
                .respond(rsipstack::sip::StatusCode::ServerInternalError, None, None)
                .await
                .ok();
            return Ok(());
        }
        info!(session_id = %session_id, action = "ivr.exec", "SIP INFO rustpbx command accepted");

        // Ack the INFO immediately.
        tx_handle
            .respond(rsipstack::sip::StatusCode::OK, None, None)
            .await
            .ok();
        Ok(())
    }

    /// Start the queue app from a resolved [`QueuePlan`].
    ///
    /// Shared by both the route→queue dispatch (`execute_flow`) and the
    /// queue-transfer path (`handle_queue_transfer`).  Resolves custom targets
    /// (skill-groups), applies the optional `queue_location_enricher`, stores
    /// the resolved plan in `pending_queue`, then starts the "queue" app.
    pub(crate) async fn start_queue_app(
        &mut self,
        mut plan: crate::call::QueuePlan,
        overflow_overrides: Option<crate::call::app::QueueOverflowOverrides>,
    ) -> Result<()> {
        use crate::call::DialStrategy;

        let agents = match &plan.dial_strategy {
            Some(strategy) => strategy.locations(),
            None => Vec::new(),
        };

        // Broadcast `queue_joined` BEFORE agent resolution so it is strictly
        // the FIRST queue event of this entry — skill-group candidate /
        // assignment events emitted during `resolve_custom_targets` below must
        // never precede it. Uses the same display id the queue app factory
        // will put into `QueueConfig::name`, so every later queue event
        // (`queue_agent_offered`, `queue_left`, ...) matches. `skill_groups`
        // lists this entry's `skill-group:` dial targets (primary first).
        let mut skill_group_ids: Vec<String> = Vec::new();
        for l in &agents {
            let uri_str = l.aor.to_string();
            if let Some(id) = uri_str.strip_prefix("skill-group:") {
                let id = id.trim().to_string();
                if !id.is_empty() && !skill_group_ids.contains(&id) {
                    skill_group_ids.push(id);
                }
            }
        }
        let joined_emitted = self.server.rwi_gateway.is_some();
        if joined_emitted {
            let gw = self.server.rwi_gateway.as_ref().unwrap().read();
            gw.broadcast(&crate::rwi::QueueJoined {
                call_id: self.context.session_id.clone(),
                queue_id: plan.display_queue_id(),
                skill_groups: (!skill_group_ids.is_empty()).then(|| skill_group_ids.clone()),
                // Absent == 0 so default-priority flows keep the exact same
                // wire payload as before the field existed.
                priority: (plan.priority != 0).then_some(plan.priority),
                // First entry into this queue app — the ACD re-entry counter
                // lives in the queue app (attempt >= 2 on agent-miss re-joins).
                attempt: None,
                agent_id: None,
            });
        }

        // Resolve custom targets (skill-groups → specific agents). Capture the
        // primary skill-group id BEFORE resolution rewrites the dial strategy —
        // the queue app factory needs it to resolve the escalation plan.
        let primary_skill_group = skill_group_ids.first().cloned();
        let resolved_agents = self
            .resolve_custom_targets_with_priority(agents, plan.priority)
            .await;

        // Enrich via queue_location_enricher if configured
        let mut queue_enricher_setup = None;
        let caller_headers: Vec<rsipstack::sip::Header> = self
            .caller_dialog
            .as_ref()
            .map(|d| d.initial_request().headers.into())
            .unwrap_or_default();
        let direction_str = self.context.dialplan.direction.to_string();
        let caller =
            crate::models::call_record::extract_sip_username(&self.context.original_caller)
                .unwrap_or_else(|| self.context.original_caller.clone());
        let callee =
            crate::models::call_record::extract_sip_username(&self.context.original_callee)
                .unwrap_or_else(|| self.context.original_callee.clone());
        let queue_id_owned = plan.queue_name.clone();
        let queue_label_owned = plan
            .label
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| queue_id_owned.clone());
        // UUI `sg=` / screen-pop context: prefer the primary skill group
        // the queue actually dials (`skill-group:{id}` target); fall back
        // to the queue id for plain queues.
        let skill_owned = primary_skill_group
            .clone()
            .unwrap_or_else(|| queue_id_owned.clone());
        // UUI `ivr=` — set by `start_ivr_app` / IVR application flow so a
        // later queue dispatch carries the originating IVR short code.
        let ivr_owned = self.session_ext_get("ivr");
        // IVR entry stamp written when the app started (see the
        // `ivr` ext above) — carries "how long in IVR" into the
        // monitor's route context.
        let ivr_entered_at = self
            .session_ext_get("ivr_entered_at")
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|t| t.with_timezone(&chrono::Utc));
        let ticket_id = other_header_ci(
            &caller_headers,
            &["X-CRM-Ticket-Id", "X-Ticket-Id", "X-CRM-Ticket"],
        );
        let customer_id = other_header_ci(&caller_headers, &["X-CRM-Customer-Id", "X-Customer-Id"]);
        let session_id_owned = self.context.session_id.to_string();
        // Queue entry stamp: wait-clock base for the monitor's meta-fallback
        // queued rows (shared `cc_acd_queue` row missing / reaped).
        let queue_entered_at = chrono::Utc::now();

        // Stamp the queue context meta UNCONDITIONALLY (not only when the
        // enricher is configured): the monitor derives the queued-state
        // fallback from these fields, so every queue entry must carry them.
        {
            let registry = self.server.active_call_registry.clone();
            let mut ctx = registry
                .get_context_meta(&session_id_owned)
                .unwrap_or_default();
            ctx.queue_id = Some(queue_id_owned.clone()).filter(|s| !s.is_empty());
            ctx.queue_name = Some(queue_label_owned.clone()).filter(|s| !s.is_empty());
            ctx.skill_group_id = Some(skill_owned.clone()).filter(|s| !s.is_empty());
            ctx.ivr_node_id = ivr_owned.clone();
            ctx.ivr_entered_at = ivr_entered_at;
            ctx.ticket_id = ticket_id.clone();
            ctx.customer_id = customer_id.clone();
            ctx.queue_entered_at = Some(queue_entered_at);
            registry.set_context_meta(session_id_owned.clone(), ctx);
        }

        let resolved_agents = if let Some(enricher) = &self.server.queue_location_enricher {
            // Capture the enrich context for the queue app: its dynamic
            // dial-list re-resolution (wait-retention assignment /
            // `resolve_agents`) must re-apply the enricher or the agent
            // INVITE loses the screen-pop headers on those paths.
            queue_enricher_setup = Some(crate::call::app::queue::QueueLocationEnricherSetup {
                enricher: enricher.clone(),
                session_id: session_id_owned.clone(),
                queue_name: queue_label_owned.clone(),
                queue_id: queue_id_owned.clone(),
                caller: caller.clone(),
                callee: callee.clone(),
                direction: direction_str.clone(),
                skill_group_id: Some(skill_owned.clone()),
                ivr_node_id: ivr_owned.clone(),
                caller_headers: caller_headers.clone(),
            });
            enricher
                .enrich(
                    resolved_agents,
                    &crate::proxy::call::QueueEnrichContext {
                        session_id: &session_id_owned,
                        queue_name: &queue_label_owned,
                        queue_id: &queue_id_owned,
                        caller: &caller,
                        callee: &callee,
                        direction: &direction_str,
                        skill_group_id: Some(&skill_owned),
                        ivr_node_id: ivr_owned.as_deref(),
                        caller_headers: &caller_headers,
                    },
                )
                .await
        } else {
            resolved_agents
        };

        let is_parallel = matches!(plan.dial_strategy, Some(DialStrategy::Parallel(_)));
        let agent_uris: Vec<String> = resolved_agents.iter().map(|l| l.aor.to_string()).collect();

        // The queue app dials from `plan.dial_strategy`, so write the resolved
        // locations back into the plan. Custom targets (skill-groups) that
        // resolved to zero reachable agents become an empty strategy — the
        // queue app then plays the busy prompt and executes its fallback
        // instead of dialing the raw `skill-group:` URI as if it were a SIP
        // contact (which would leave the caller on hold music forever).
        plan.dial_strategy = Some(if is_parallel {
            DialStrategy::Parallel(resolved_agents)
        } else {
            DialStrategy::Sequential(resolved_agents.into_iter().map(Into::into).collect())
        });

        info!(session_id = %self.id,
            queue = %plan.queue_name,
            agents = agent_uris.len(),
            parallel = is_parallel,
            "Starting queue app"
        );

        // Authoritative queue metadata for hooks, CDR, abandon detection and
        // cc_* / RWI webhook correlation (`session_hook_ctx` reads CallMeta).
        let queue_id = if !plan.queue_name.is_empty() {
            plan.queue_name.clone()
        } else if let Some(label) = plan.label.as_ref().filter(|s| !s.is_empty()) {
            label.clone()
        } else {
            String::new()
        };
        if !queue_id.is_empty() {
            self.meta.queue_name = Some(queue_id);
        }
        if let Some(label) = plan.label.as_ref().filter(|s| !s.is_empty()) {
            self.meta.queue_label = Some(label.clone());
        }
        if let Some(sg) = primary_skill_group.as_ref().filter(|s| !s.is_empty()) {
            self.meta.skill_group_id = Some(sg.clone());
        }

        let has_resolved_agents = !agent_uris.is_empty();

        // PBX-wide queue wait music (`[proxy] queue_hold_music`): the
        // queue's own `[queues.<name>.hold]` always wins; the global default
        // fills the gap so callers wait with music instead of bare ringback.
        // Single choke point — every queue ingress (route rule, skill-group
        // ACD, consult-to-queue transfer) funnels through this plan.
        crate::call::apply_queue_hold_music_default(
            &mut plan,
            self.server.proxy_config.load().queue_hold_music.clone(),
        );

        // A skill-group target enables skill routing in the queue app, which
        // re-resolves the group in `on_enter` — so the app may dial agents the
        // pre-resolution below did not see. Captured before the move.
        let is_skill_group_queue = primary_skill_group.is_some();

        // Store resolved plan in context for the queue app factory
        if let Some(ctx) = self.app_runtime.app_context() {
            *ctx.pending_queue.lock() = Some(PendingQueuePlan {
                plan: plan.clone(),
                agent_uris,
                parallel: is_parallel,
                skill_group_id: primary_skill_group,
                overflow_overrides,
                joined_emitted,
                queue_enricher: queue_enricher_setup,
            });
        }

        if let Err(e) = self
            .ensure_app_running_with(
                "queue",
                None,
                plan.accept_immediately,
                &format!("queue '{}'", plan.queue_name),
                None,
            )
            .await
        {
            let info = &crate::proxy::proxy_call::error_catalog::QUEUE_START_FAILED;
            self.meta.error_code = Some(info);
            let detail = serde_json::json!({
                "queue": plan.queue_name,
                "error": format!("{e:?}"),
            });
            self.record_trace(crate::call_errors::call_error_trace(
                info,
                Some(detail.clone()),
            ));
            self.emit_typed_rwi_event(&crate::rwi::CallError {
                call_id: self.context.session_id.clone(),
                session_id: Some(self.context.session_id.clone()),
                stage: "queue".to_string(),
                app: info.app.to_string(),
                code: info.code.to_string(),
                severity: info.severity.as_str().to_string(),
                message: info.message.to_string(),
                sip_status: info.sip_status,
                detail: Some(detail),
            });
            // Avoid an orphan `queue_joined`: with the queue app never
            // started, no `queue_left` would ever close the lifecycle.
            if joined_emitted && let Some(gw) = self.server.rwi_gateway.as_ref() {
                gw.read().broadcast(&crate::rwi::QueueLeft {
                    call_id: self.context.session_id.clone(),
                    queue_id: plan.display_queue_id(),
                    reason: Some("start_failed".to_string()),
                    skill_groups: None,
                });
            }
            return Err(anyhow!("Failed to start queue app: {:?}", e));
        }

        // The queue app now drives the session. Attribute the terminal phase to
        // the queue and record the queue entry so the call trace shows the full
        // timeline (IVR → … → Entered queue → caller abandoned → end) instead
        // of jumping straight from "answered" to "queue.abandoned".
        self.meta.app_name = Some("queue".to_string());
        let queue_name = plan.queue_name.clone();
        let queue_label = self.meta.queue_label.clone().unwrap_or_default();
        let skill_group = self.meta.skill_group_id.clone().unwrap_or_default();
        let mut queue_detail = serde_json::json!({ "queue_name": queue_name });
        if !queue_label.is_empty() && queue_label != queue_name {
            queue_detail["queue_label"] = serde_json::Value::String(queue_label);
        }
        if !skill_group.is_empty() {
            queue_detail["skill_group"] = serde_json::Value::String(skill_group);
        }
        self.record_trace(
            crate::call_errors::TraceEvent::new(
                crate::call_errors::TraceKind::Queue,
                if queue_name.is_empty() {
                    "Entered queue".to_string()
                } else {
                    format!("Entered queue '{}'", queue_name)
                },
            )
            .severity(crate::call_errors::ErrSeverity::Info)
            .detail(queue_detail),
        );

        // Inject dial_next_agent to kick off sequential agent dialing
        // (parallel mode auto-dials in on_enter).
        //
        // The kick must also fire when a skill-group target pre-resolved to
        // ZERO agents: `on_enter` re-resolves the group against the live
        // registry (`resolve_agents`), so an agent that flipped to Idle in the
        // gap between pre-resolution and app start would otherwise be put on
        // the sequential dial list with nothing to trigger the first INVITE —
        // the caller sat on hold music until abandoning while the agent stayed
        // Idle. Harmless when the re-resolve ALSO finds nothing: the app is
        // already in wait retention and `dial_next_agent` no-ops there.
        //
        // Plain (non-skill-group) queues that resolved no agents still skip the
        // kick — `on_enter` fell through to the busy-prompt/fallback path, so a
        // second dial attempt would replay the busy prompt.
        if !is_parallel && (has_resolved_agents || is_skill_group_queue) {
            let (name, data) = QueueSignal::DialNext.to_custom();
            let _ = self.app_runtime.inject_event(serde_json::json!({
                "type": "custom",
                "name": name,
                "data": data,
            }));
        }

        Ok(())
    }

    /// Start the queue app from `CallCommand::StartApp("queue", params)` app
    /// params — see [`crate::call::QueuePlan::from_app_params`] for the
    /// accepted target forms. The optional ACD `priority` is stored as a
    /// session extension (`queue_priority`) for ACD/pacing consumers; the
    /// plan itself carries the dial target only.
    pub(crate) async fn start_queue_app_from_params(
        &mut self,
        params: Option<&serde_json::Value>,
    ) -> Result<()> {
        let Some(params) = params else {
            anyhow::bail!("queue app requires params with a `queue` target");
        };
        let (plan, priority) = crate::call::QueuePlan::from_app_params(params)?;
        let plan =
            plan.with_default_hold_music(self.server.proxy_config.load().queue_hold_music.clone());
        if let Some(priority) = priority {
            self.session_ext_set("queue_priority", priority.to_string());
        }
        self.start_queue_app(plan, None).await
    }

    pub(crate) async fn execute_queue_recovery_dial(
        &mut self,
        request_id: String,
        app_execution_id: u64,
        deadline: tokio::time::Instant,
        command: CallCommand,
    ) -> CommandResult {
        let capability_check = self.check_capability(&command);
        let CallCommand::LegAdd {
            target,
            leg_id: Some(leg_id),
            headers,
            source_leg,
            purpose: Some(crate::call::domain::LegPurpose::Agent),
        } = command
        else {
            return CommandResult::failure("invalid_recovery_command");
        };
        let rejection = if tokio::time::Instant::now() >= deadline {
            Some("request_expired")
        } else {
            self.queue_recovery_guard(Some(app_execution_id))
                .await
                .err()
        };
        let result = if let Some(reason) = rejection {
            CommandResult::failure(reason)
        } else if let MediaCapabilityCheck::Denied { reason } = capability_check {
            CommandResult::failure_with_kind(
                format!("media capability denied: {reason}"),
                CommandFailureKind::MediaDenied,
            )
        } else {
            let headers = headers
                .into_iter()
                .map(|(name, value)| rsipstack::sip::headers::make_header(&name, value))
                .collect();
            let source_leg = source_leg.map(|id| {
                if id.as_str() == "callee" {
                    self.resolve_transfer_leg()
                } else {
                    id
                }
            });
            // Bound locator, route and media waits too; an accepted queue command
            // is not permission to originate after its diagnostic request expires.
            let provisional_leg = self.legs.get(&leg_id).is_none();
            let outcome = tokio::time::timeout_at(
                deadline,
                self.add_leg_body(
                    target,
                    Some(leg_id.clone()),
                    headers,
                    source_leg,
                    Some(crate::call::domain::LegPurpose::Agent),
                    None,
                    Some(deadline),
                ),
            )
            .await;
            match outcome {
                Ok(Ok(id)) => CommandResult::success_with_leg(id),
                failed => {
                    // Removing the provisional leg also stops media and aborts
                    // its INVITE task before the queue can release the reservation.
                    if provisional_leg && self.legs.remove(&leg_id).is_some() {
                        self.record_leg_event(
                            &leg_id.0,
                            crate::callrecord::LegTimelineEventType::Removed,
                            None,
                            Some(serde_json::json!({ "reason": "recovery_dial_canceled" })),
                        );
                    }
                    match failed {
                        Ok(Err(error)) => CommandResult::failure(error.to_string()),
                        Err(_) => CommandResult::failure("request_expired"),
                        Ok(Ok(_)) => unreachable!(),
                    }
                }
            }
        };
        if !result.success {
            let reason = result.message.as_deref().unwrap_or("recovery_dial_failed");
            warn!(session_id = %self.id, %request_id, %leg_id, app_execution_id, reason,
                leg = "agent", direction = "outbound", "Queue: guarded recovery dial rejected before SIP origination");
            self.app_event_bridge.send_app_event(crate::call::app::ControllerEvent::Custom(
                "queue_recovery_dial_failed".into(),
                serde_json::json!({"request_id": request_id, "leg_id": leg_id.0, "reason": reason}),
            ));
        }
        result
    }

    pub(crate) async fn process_start_app_command(
        &mut self,
        app_name: String,
        params: Option<serde_json::Value>,
        auto_answer: bool,
        route_context: Option<crate::call::app::AppRouteContext>,
    ) -> CommandResult {
        // A new app replaces any active realtime bridge (one media-driving app per session).
        self.teardown_realtime_bridge("app replaced");
        // Command-path queue start: RWI `app.start` / outbound `on_answer: enqueue`
        // carry the queue target in params instead of a dialplan-resolved plan.
        if app_name == "queue"
            && params
                .as_ref()
                .is_some_and(|value| value.get("queue").is_some())
        {
            return match self.start_queue_app_from_params(params.as_ref()).await {
                Ok(()) => {
                    self.sync_rtp_timeout_pause();
                    CommandResult::success()
                }
                Err(error) => CommandResult::failure(error.to_string()),
            };
        }
        let result = if let Some(context) = route_context {
            self.app_runtime
                .start_app_with_route_context(&app_name, params, auto_answer, context)
                .await
        } else {
            self.app_runtime
                .start_app(&app_name, params, auto_answer)
                .await
        };
        match result {
            Ok(()) => {
                self.bind_suspended_ivr_exec_to_current_app().await;
                self.sync_rtp_timeout_pause();
                CommandResult::success()
            }
            Err(error) => {
                let reason = error.to_string();
                self.fail_ivr_exec_start(reason.clone()).await;
                CommandResult::failure(reason)
            }
        }
    }

    pub(crate) fn queue_live_agent_legs(&self) -> usize {
        self.legs.iter().filter(|(id, leg)| {
            if id.as_str() == "caller" || leg.state == LegState::Ended {
                return false;
            }
            // Session construction reserves a blank callee slot for media wiring.
            // It is not a dial; every other initializing or attached leg blocks recovery.
            !(id.as_str() == "callee" && leg.state == LegState::Initializing
                && leg.endpoint.is_none() && leg.agent_id.is_none() && leg.purpose.is_none()
                && self.legs.get_dialog(id).is_none())
        }).count()
    }

    pub(crate) async fn queue_recovery_guard(&self, expected_generation: Option<u64>) -> Result<u64, &'static str> {
        if self.app_runtime.current_app().as_deref() != Some("queue") {
            return Err("not_queue");
        }
        if self.meta.transfer_in_progress {
            return Err("transfer_in_progress");
        }
        if !self.legs.get(&LegId::from("caller"))
            .is_some_and(|leg| matches!(leg.state, LegState::Connected | LegState::Hold)) {
            return Err("caller_not_connected");
        }
        if self.queue_live_agent_legs() > 0 {
            return Err("active_agent_leg");
        }
        let generation = self.app_runtime.current_app_invocation().await
            .ok_or("queue_generation_unavailable")?.app_execution_id;
        if expected_generation.is_some_and(|expected| expected != generation) {
            return Err("queue_generation_changed");
        }
        Ok(generation)
    }

    /// Handle app exit: iterate hooks and run post-exit actions (unhold, send result INFO).
    pub(crate) async fn handle_app_exited(&mut self, app_execution_id: Option<u64>) -> CommandResult {
        if self.server.session_hooks.is_empty() {
            return CommandResult::success();
        }

        let ctx = self.session_hook_ctx();

        // Collect completion intents from hooks while holding only an immutable
        // reference to self, then apply them with a mutable reference.
        let completions: Vec<_> = {
            let hooks = self.server.session_hooks.clone();
            let mut results = Vec::new();
            for hook in hooks.iter() {
                if let Some(completion) = hook.on_app_exited(&ctx, app_execution_id).await {
                    results.push(completion);
                }
            }
            results
        };

        for completion in completions {
            // Restore the exact leg on which handle_hold negotiated SDP.
            if let Some(leg_id) = &completion.unhold_leg {
                if self.legs.get(leg_id).is_some() {
                    if let Err(e) = self.handle_unhold(leg_id.clone()).await {
                        warn!(session_id = %self.id,
                            session_id = %self.context.session_id,
                            leg = %leg_id,
                            error = %e,
                            "Failed to unhold leg after app exit"
                        );
                    }
                }
            }

            if let Some(mut hints) = completion.route_hints {
                let holds = std::mem::take(&mut hints.concurrency_holds);
                if !holds.is_empty()
                    && let Some(limiter) = self.server.frequency_limiter.as_ref()
                {
                    crate::call::policy::PolicyGuard::release_concurrency_holds(
                        &holds,
                        limiter.as_ref(),
                    )
                    .await;
                }
                // Dropping the invocation-owned concurrent-call lease here
                // releases it at app exit instead of retaining it for the call.
                drop(hints);
            }

            // Send result INFO if requested.
            if let Some(spec) = completion.result_info {
                if let Err(e) = self
                    .handle_send_info(spec.leg_id, spec.content_type, spec.body)
                    .await
                {
                    warn!(session_id = %self.id,
                        session_id = %self.context.session_id,
                        error = %e,
                        "Failed to send result INFO after app exit"
                    );
                }
            }
        }

        CommandResult::success()
    }

    /// Unified post-disconnect handler: decide what to do with the caller after
    /// the connected B-leg (agent / bridge) terminates.
    ///
    /// Precedence (CSAT-first):
    /// 1. `on_agent_disconnected` hooks (e.g. CSAT survey) — first hook that
    ///    returns `true` takes over and this method returns.
    /// 2. `meta.transfer_return_app` — start the stored return app.
    /// 3. Neither — queue a normal hangup.
    pub(super) async fn handle_start_return_app(&mut self) -> CommandResult {
        let caller_alive = self.legs.contains_key(&LegId::from("caller"))
            && !self
                .caller_dialog
                .as_ref()
                .is_none_or(|d| d.state().is_terminated());

        if !caller_alive {
            // An outbound transfer retired the caller/agent, not the customer;
            // the surviving primary dialog still needs the target's hangup.
            if self.meta.transferred && !self.legs.contains_key(&LegId::from("caller")) {
                // The transferor's BYE may still be in flight, so its dialog
                // cannot identify the surviving customer yet.
                if let Some(dialog_id) = self.meta.connected_callee_dialog_id.clone() {
                    self.pending_hangup.insert(dialog_id);
                }
            }
            // Caller died while the flow was suspended on a bridge/queue —
            // the suspended IVR will never resume, so emit the compensating
            // session_end trace it suppressed at hand-off time.
            if self.meta.ivr_flow_suspended {
                self.meta.ivr_flow_suspended = false;
                self.emit_suspended_flow_session_end(
                    crate::call::app::ivr::provider::SessionEndReason {
                        reason: crate::call::app::ivr::provider::SessionEndTag::UserHangup,
                        detail: None,
                    },
                )
                .await;
                self.complete_suspended_ivr_exec("user_hangup").await;
            }
            self.meta.pending_transfer_outcome = None;
            return CommandResult::success();
        }

        // A supervisor takeover intentionally ended the agent (B) leg — the
        // customer stays alive in the takeover conference with the supervisor,
        // so the regular B-leg-disconnect cascade (return app / caller hangup)
        // must not run.
        if self.meta.supervisor_takeover_active {
            info!(session_id = %self.id,
                "B-leg ended by supervisor takeover; keeping caller in takeover conference"
            );
            return CommandResult::success();
        }

        if self.deliver_pending_transfer_result() {
            return CommandResult::success();
        }

        // 1. CSAT / session hooks take precedence.
        let ctx = self.session_hook_ctx();
        for hook in self.server.session_hooks.iter() {
            if hook.on_agent_disconnected(&ctx, &*self.app_runtime).await {
                return CommandResult::success();
            }
        }

        // 2. Return app from CallMeta.
        if let Some(mut spec) = self.meta.transfer_return_app.take() {
            info!(session_id = %self.id,
                app = %spec.app_name,
                "B‑leg disconnected; starting return app"
            );
            // Replay DTMF captured during the bridge into the return IVR's
            // params so the resumed flow (and its step provider) can see the
            // keys the caller pressed while the bridge owned the media.
            let digits: Vec<String> = std::mem::take(&mut *self.bridge_dtmf_digits.lock());
            // Consume the bridge-close snapshot unconditionally so it can
            // never leak into an unrelated later return (CSAT / agent
            // disconnect with no bridge in between).
            let resumed_bridge_ctx = self.resumed_bridge_ctx.lock().take();
            if spec.app_name == "ivr"
                && let Some(obj) = spec.params.as_object_mut()
            {
                let ivr_params = obj
                    .entry("ivr_params")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(ip) = ivr_params.as_object_mut() {
                    if !digits.is_empty() {
                        ip.insert(
                            "bridge_dtmf_digits".to_string(),
                            serde_json::Value::String(digits.join(",")),
                        );
                    }
                    // Originating bridge-step context (serialized —
                    // `with_ivr_params` keeps string values only): attached
                    // on EVERY bridge return, not just digit-carrying ones —
                    // a no-digit resume needs it to self-describe
                    // (`resume_from_step_id` in the provider request) so the
                    // provider can continue the flow instead of re-entering
                    // it. Snapshot taken at `VoipBridgeClosed`, consumed here.
                    if let Some(ctx) = resumed_bridge_ctx
                        && let Ok(s) = serde_json::to_string(&ctx)
                    {
                        ip.insert("bridge_step_ctx".to_string(), serde_json::Value::String(s));
                    }
                }
            }
            self.bridge.clear();
            let label = format!("Return app '{}'", spec.app_name);
            match self
                .ensure_app_running(&spec.app_name, Some(spec.params), &label)
                .await
            {
                Ok(()) => {
                    // Flow resumed — the successor app owns the lifecycle now.
                    self.bind_suspended_ivr_exec_to_current_app().await;
                    self.meta.ivr_flow_suspended = false;
                    return CommandResult::success();
                }
                Err(e) => {
                    warn!(session_id = %self.id,
                        app = %spec.app_name,
                        error = %e,
                        "Failed to start return app; falling through to hangup"
                    );
                    if self.meta.ivr_flow_suspended {
                        self.meta.ivr_flow_suspended = false;
                        self.emit_suspended_flow_session_end(
                            crate::call::app::ivr::provider::SessionEndReason {
                                reason: crate::call::app::ivr::provider::SessionEndTag::Error,
                                detail: Some(e.to_string()),
                            },
                        )
                        .await;
                        self.complete_suspended_ivr_exec("return_app_start_failed")
                            .await;
                    }
                }
            }
        }

        // 3. Neither hook nor return app — hang up the caller.
        self.pending_hangup.insert(self.caller_dialog_id());
        CommandResult::success()
    }
}
