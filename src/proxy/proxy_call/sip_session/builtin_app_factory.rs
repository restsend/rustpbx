use super::prelude::*;
use crate::call::runtime::AppFactory;

pub(crate) struct BuiltinAppFactory {
    pub(crate) addon_registry: Option<Arc<crate::addons::registry::AddonRegistry>>,
    /// Server-level agent registry, handed to the QueueApp so it can resolve
    /// the answering agent's display name for the service prompt.
    pub(crate) agent_registry: Option<Arc<dyn crate::call::app::agent_registry::AgentRegistry>>,
}

impl BuiltinAppFactory {
    pub(crate) fn new(
        addon_registry: Option<Arc<crate::addons::registry::AddonRegistry>>,
        agent_registry: Option<Arc<dyn crate::call::app::agent_registry::AgentRegistry>>,
    ) -> Self {
        Self {
            addon_registry,
            agent_registry,
        }
    }
}

#[async_trait]
impl AppFactory for BuiltinAppFactory {
    async fn create_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        context: &ApplicationContext,
    ) -> Result<Option<Box<dyn crate::call::app::CallApp>>, anyhow::Error> {
        let mut diagnostic = None;
        let app = self
            .build_app(app_name, params, context, &mut diagnostic)
            .await;
        match diagnostic {
            Some(msg) => Err(anyhow::anyhow!(msg)),
            None => Ok(app),
        }
    }
}

impl BuiltinAppFactory {
    fn ivr_fallback_configured(context: &ApplicationContext) -> bool {
        context
            .config
            .proxy
            .ivr_fallback
            .as_ref()
            .is_some_and(|c| c.is_configured())
    }

    fn ivr_fallback_arc(
        context: &ApplicationContext,
    ) -> Option<std::sync::Arc<crate::config::IvrFallbackConfig>> {
        context
            .config
            .proxy
            .ivr_fallback
            .as_ref()
            .filter(|c| c.is_configured())
            .map(|c| std::sync::Arc::new(c.clone()))
    }

    fn effective_ivr_fallback(
        context: &ApplicationContext,
        project: Option<&crate::config::IvrFallbackConfig>,
    ) -> Option<std::sync::Arc<crate::config::IvrFallbackConfig>> {
        if let Some(config) = project.filter(|c| c.is_configured()) {
            return Some(std::sync::Arc::new(config.clone()));
        }
        Self::ivr_fallback_arc(context)
    }

    fn prefer_session_ivr_fallback(
        context: &ApplicationContext,
        project: Option<&crate::config::IvrFallbackConfig>,
    ) -> bool {
        project.is_some_and(|c| c.is_configured()) || Self::ivr_fallback_configured(context)
    }

    fn build_tree_ivr_app(
        definition: crate::call::app::ivr::IvrDefinition,
        params: Option<&serde_json::Value>,
    ) -> Box<dyn crate::call::app::CallApp> {
        let mut app = crate::call::app::ivr::IvrApp::new(definition);
        if let Some(tts_value) = params.and_then(|p| p.get("tts"))
            && let Ok(tts_cfg) = serde_json::from_value::<crate::tts::TtsConfig>(tts_value.clone())
        {
            app = app.with_tts(Some(tts_cfg));
        }
        if let Some(ivp) = params.and_then(|p| p.get("ivr_params")) {
            if let Some(menu) = ivp.get("return_menu").and_then(|v| v.as_str()) {
                if !menu.is_empty() {
                    app = app.with_start_menu(menu.to_string());
                }
            }
        }
        Box::new(app)
    }

    async fn build_app(
        &self,
        app_name: &str,
        params: Option<serde_json::Value>,
        context: &ApplicationContext,
        diagnostic: &mut Option<String>,
    ) -> Option<Box<dyn crate::call::app::CallApp>> {
        // First try addon hooks (allows addons to override built-in apps).
        if let Some(reg) = &self.addon_registry {
            if let Some(app) = reg.build_call_app(app_name, params.clone(), context).await {
                return Some(app);
            }
        }
        match app_name {
            "rwi" => {
                let Some(gateway) = context.rwi_gateway.clone() else {
                    *diagnostic = Some("RWI gateway is not configured".into());
                    return None;
                };
                let context_name = params.as_ref()
                    .and_then(|p| p.get("context"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");
                Some(Box::new(crate::call::app::rwi::RwiApp::new(
                    context_name.to_owned(), gateway,
                )))
            }
            "realtime" => {
                // Realtime (AI voice) bridge — presets resolve from
                // [[realtime]] config; the api_key never rides app params.
                let app = crate::call::app::realtime::RealtimeApp::from_params(
                    params.as_ref(),
                    context.config.realtime.as_deref(),
                )
                .map_err(|e| {
                    *diagnostic = Some(format!("realtime app: {e}"));
                })
                .ok()?;
                Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
            }
            "ivr" => {
                // Record the IVR config file path so pending return specs
                // (sub-app hand-offs) can resume the same IVR later.
                if let Some(file) = params
                    .as_ref()
                    .and_then(|p| p.get("file"))
                    .and_then(|v| v.as_str())
                {
                    crate::call::app::ivr::exec::remember_ivr_start_file(context, file);
                }
                // First check if params has inline step mode config (legacy/debug routes)
                let mode = params
                    .as_ref()
                    .and_then(|p| p.get("mode").and_then(|v| v.as_str()))
                    .unwrap_or(crate::config::DEFAULT_IVR_MODE);

                if mode == "step" && params.as_ref()?.get("url").is_some() {
                    // Inline step mode (from debug routes or legacy app_params)
                    let url = params
                        .as_ref()
                        .and_then(|p| p.get("url").and_then(|v| v.as_str()))?;

                    let mut provider =
                        crate::call::app::ivr::StepProvider::new(url, context.http_client.clone());

                    if let Some(hdrs) = params.as_ref()?.get("headers") {
                        if let Some(h) = hdrs.as_object() {
                            for (k, v) in h {
                                if let Some(vs) = v.as_str() {
                                    provider.add_header(k, vs);
                                }
                            }
                        }
                    }

                    if let Some(retry) = params.as_ref()?.get("retry") {
                        let max_retries = retry
                            .get("max_retries")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(3) as u32;
                        let timeout = retry
                            .get("timeout_ms")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(1000);
                        let retry_delay = retry
                            .get("delay_ms")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(100);
                        let fallback = serde_json::from_value(
                            retry.get("fallback").cloned().unwrap_or(serde_json::json!({
                                "type": "hangup",
                                "prompt": "sounds/error.wav"
                            })),
                        )
                        .ok();
                        provider = provider.with_retry(crate::call::app::ivr::RetryConfig {
                            max_retries,
                            timeout_ms: timeout,
                            retry_delay_ms: retry_delay,
                            fallback_action: fallback,
                        });
                    }
                    if Self::prefer_session_ivr_fallback(context, None) {
                        provider = provider.with_prefer_ivr_fallback(true);
                    }

                    let mut app =
                        crate::call::app::ivr::StepIvrApp::with_provider(Box::new(provider));
                    let ivr_name = params
                        .as_ref()
                        .and_then(|p| p.get("name").and_then(|v| v.as_str()))
                        .unwrap_or("step_ivr")
                        .to_string();
                    app = app.with_name(ivr_name);
                    app = app.with_route_name(context.call_info.route_name.clone());
                    if let Some(repeat) = params
                        .as_ref()
                        .and_then(|p| p.get("max_repeat_prompts").and_then(|v| v.as_u64()))
                    {
                        app = app.with_max_repeat_prompts(repeat as u32);
                    }
                    if let Some(max_restarts) = params
                        .as_ref()
                        .and_then(|p| p.get("max_flow_restarts").and_then(|v| v.as_u64()))
                    {
                        app = app.with_max_flow_restarts(max_restarts as u32);
                    }
                    if let Some(window) = params
                        .as_ref()
                        .and_then(|p| p.get("reentry_window_secs").and_then(|v| v.as_u64()))
                    {
                        app = app.with_reentry_window_secs(window);
                    }
                    if let Some(tts_value) = params.as_ref()?.get("tts")
                        && let Ok(tts_cfg) =
                            serde_json::from_value::<crate::tts::TtsConfig>(tts_value.clone())
                    {
                        app = app.with_tts(Some(tts_cfg));
                    }
                    app = app.with_rwi_gateway(context.rwi_gateway.clone());
                    app = app.with_trace(context.ivr_trace.clone());
                    if let Some(ivp) = params.as_ref().and_then(|p| p.get("ivr_params")) {
                        app = app.with_ivr_params(ivp.clone());
                        if let Some(tf) = ivp.get("transferred_from").and_then(|v| v.as_str()) {
                            app = app.with_transferred_from(Some(tf.to_string()));
                        }
                    }
                    app = app.with_ivr_fallback(Self::ivr_fallback_arc(context));
                    Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
                } else {
                    // File-based: read TOML and detect mode from content.
                    // Supports both filesystem paths and virtual `db://<category>/<name>` URIs
                    // produced by `resolve_ivr_file` / `apply_route_metadata`
                    // when the proxy runs with `generated_db = true`.
                    let file = params.as_ref()?.get("file")?.as_str()?;

                    if let Some(builtin_name) = crate::call::app::ivr::builtin::parse_uri(file) {
                        match crate::call::app::ivr::builtin::get(builtin_name) {
                            Some(defn) => {
                                return Some(Self::build_tree_ivr_app(defn, params.as_ref()));
                            }
                            None => {
                                *diagnostic = Some(format!("unknown builtin IVR '{builtin_name}'"));
                                return None;
                            }
                        }
                    }

                    let content = if let Some((category, name)) =
                        crate::config_store::GeneratedConfigStore::parse_db_uri(file)
                    {
                        let store = crate::config_store::GeneratedConfigStore::from_config(
                            &context.config,
                            &context.db,
                        );
                        match store.read(category, name).await {
                            Ok(Some(c)) => c,
                            Ok(None) => {
                                tracing::warn!("IVR config '{}' not found in config store", file);
                                *diagnostic = Some(format!(
                                    "IVR config '{}' not found in config store",
                                    file
                                ));
                                return None;
                            }
                            Err(e) => {
                                warn!("Failed to read IVR config '{}' from store: {}", file, e);
                                *diagnostic = Some(format!(
                                    "Failed to read IVR config '{}' from store: {}",
                                    file, e
                                ));
                                return None;
                            }
                        }
                    } else {
                        match tokio::fs::read_to_string(file).await {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!("Failed to read IVR config '{}': {}", file, e);
                                *diagnostic =
                                    Some(format!("Failed to read IVR config '{}': {}", file, e));
                                return None;
                            }
                        }
                    };

                    let file_config: crate::call::app::ivr_config::IvrFileConfig =
                        match toml::from_str(&content) {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!("Failed to parse IVR TOML '{}': {}", file, e);
                                *diagnostic =
                                    Some(format!("Failed to parse IVR TOML '{}': {}", file, e));
                                return None;
                            }
                        };

                    if file_config.ivr.is_step_mode() {
                        // Step mode from TOML
                        let provider_cfg = file_config.ivr.provider.as_ref()?;
                        let provider_url =
                            match provider_cfg.resolve_url(&context.config.proxy.ivr_endpoints) {
                                Ok(url) => url,
                                Err(error) => {
                                    *diagnostic = Some(error);
                                    return None;
                                }
                            };
                        let mut provider = crate::call::app::ivr::StepProvider::new(
                            &provider_url,
                            context.http_client.clone(),
                        );
                        for (k, v) in &provider_cfg.headers {
                            provider.add_header(k, v);
                        }
                        provider = provider
                            .with_retry(crate::call::app::ivr::RetryConfig::from(provider_cfg));
                        if let Some(mode) = provider_cfg
                            .resume_event_mode
                            .as_deref()
                            .and_then(
                                crate::call::app::ivr::ResumeEventMode::parse,
                            )
                        {
                            provider = provider.with_resume_event_mode(mode);
                        } else if let Some(raw) = provider_cfg.resume_event_mode.as_deref() {
                            tracing::warn!(
                                value = %raw,
                                "invalid ivr provider resume_event_mode (expected \"resume\" or \"session_start\"); using default \"resume\""
                            );
                        }
                        if Self::prefer_session_ivr_fallback(
                            context,
                            file_config.ivr.ivr_fallback.as_ref(),
                        ) {
                            provider = provider.with_prefer_ivr_fallback(true);
                        }

                        let mut app =
                            crate::call::app::ivr::StepIvrApp::with_provider(Box::new(provider));
                        app = app.with_name(file_config.ivr.name.clone());
                        app = app.with_route_name(context.call_info.route_name.clone());
                        if let Some(repeat) = params
                            .as_ref()
                            .and_then(|p| p.get("max_repeat_prompts").and_then(|v| v.as_u64()))
                        {
                            app = app.with_max_repeat_prompts(repeat as u32);
                        }
                        if let Some(max_restarts) = params
                            .as_ref()
                            .and_then(|p| p.get("max_flow_restarts").and_then(|v| v.as_u64()))
                        {
                            app = app.with_max_flow_restarts(max_restarts as u32);
                        }
                        if let Some(window) = params
                            .as_ref()
                            .and_then(|p| p.get("reentry_window_secs").and_then(|v| v.as_u64()))
                        {
                            app = app.with_reentry_window_secs(window);
                        }
                        if let Some(tts_value) = params.as_ref()?.get("tts")
                            && let Ok(tts_cfg) =
                                serde_json::from_value::<crate::tts::TtsConfig>(tts_value.clone())
                        {
                            app = app.with_tts(Some(tts_cfg));
                        }
                        app = app.with_rwi_gateway(context.rwi_gateway.clone());
                        app = app.with_trace(context.ivr_trace.clone());
                        if let Some(ivp) = params.as_ref().and_then(|p| p.get("ivr_params")) {
                            app = app.with_ivr_params(ivp.clone());
                            if let Some(tf) = ivp.get("transferred_from").and_then(|v| v.as_str()) {
                                app = app.with_transferred_from(Some(tf.to_string()));
                            }
                        }
                        app = app.with_ivr_fallback(Self::effective_ivr_fallback(
                            context,
                            file_config.ivr.ivr_fallback.as_ref(),
                        ));
                        Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
                    } else {
                        // Tree mode from TOML
                        Some(Self::build_tree_ivr_app(file_config.ivr, params.as_ref()))
                    }
                }
            }
            "voicemail" => {
                let extension = params.as_ref()?.get("extension")?.as_str()?.to_string();
                // Core voicemail fallback — addon overrides via build_call_app above.
                // Greeting: dialplan `greeting_path` param > `[proxy]
                // voicemail_greeting` > built-in default.
                let greeting = crate::call::app::voicemail::resolve_greeting_path(
                    params
                        .as_ref()
                        .and_then(|p| p.get("greeting_path"))
                        .and_then(|v| v.as_str()),
                    context.config.proxy.voicemail_greeting.as_deref(),
                );
                let app = crate::call::app::voicemail::VoicemailApp::new(extension)
                    .with_greeting_path(greeting);
                Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
            }
            "conference" => {
                let conf_id = params
                    .as_ref()?
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default")
                    .to_string();
                let caller_id = context.call_info.caller.clone();
                Some(Box::new(crate::call::app::conference::ConferenceApp::new(
                    conf_id, caller_id,
                )) as Box<dyn crate::call::app::CallApp>)
            }
            "queue" => {
                let pending = context.pending_queue.lock().take()?;
                let mut plan = pending.plan;
                let mut config = crate::call::app::queue::QueueConfig::default();
                // Same display id the session broadcast in the early
                // `queue_joined` (label first, else queue name).
                config.name = plan.display_queue_id();
                config.accept_immediately = plan.accept_immediately;
                config.hold = plan.hold.clone();
                config.fallback = plan.fallback.clone();
                config.voice_prompts = plan.voice_prompts.clone();
                config.ring_timeout = plan.ring_timeout;
                // Per-call ACD priority from app params / plan (0 = default).
                config.priority = plan.priority;
                // Build agent locations from resolved URIs
                let agents: Vec<crate::call::Location> = pending
                    .agent_uris
                    .iter()
                    .map(|uri| {
                        let aor: rsipstack::sip::Uri = uri
                            .parse()
                            .unwrap_or_else(|_| format!("sip:{}", uri).parse().unwrap_or_default());
                        crate::call::Location {
                            aor,
                            contact_raw: Some(uri.clone()),
                            ..Default::default()
                        }
                    })
                    .collect();
                config.agents = agents.clone();
                config.strategy = if pending.parallel {
                    crate::call::DialStrategy::Parallel(agents)
                } else {
                    crate::call::DialStrategy::Sequential(agents)
                };

                // Skill-group queues: enable lifecycle events + wait retention.
                if let Some(ref sg) = pending.skill_group_id {
                    config.skill_routing_enabled = true;
                    config.skill_group = Some(sg.clone());
                    if let Some(ref registry) = self.agent_registry {
                        if let Some((skills, max_wait, retry)) =
                            registry.skill_group_queue_config(sg).await
                        {
                            if config.required_skills.is_empty() {
                                config.required_skills = skills;
                            }
                            config.max_wait_secs = max_wait;
                            config.retry_interval_secs = retry;
                        }
                    }
                    // URI `overflow_wait=` overrides the skill-group's
                    // `max_wait_secs` (queue-level fallback timeout).
                    if let Some(ovr) = pending.overflow_overrides.as_ref() {
                        if let Some(wait) = ovr.max_wait_secs {
                            config.max_wait_secs = wait;
                        }
                    }
                    // Ensure hold + comfort (wait retention) have audio.
                    if plan.hold.is_none() {
                        plan.hold = Some(
                            crate::call::QueueHoldConfig::default()
                                .with_audio_file(crate::call::DEFAULT_QUEUE_HOLD_AUDIO.to_string()),
                        );
                        config.hold = plan.hold.clone();
                    }
                    if plan.voice_prompts.is_none() {
                        let mut prompts = crate::call::VoicePrompts::zh();
                        if prompts.comfort_prompts.is_empty() {
                            if let Some(ref busy) = prompts.busy_prompt {
                                prompts.comfort_prompts.push(crate::call::ComfortPrompt {
                                    audio_file: busy.clone(),
                                    interval_secs: 30,
                                });
                            }
                        }
                        plan.voice_prompts = Some(prompts.clone());
                        config.voice_prompts = Some(prompts);
                    } else if let Some(ref mut prompts) = plan.voice_prompts {
                        if prompts.comfort_prompts.is_empty() {
                            if let Some(busy) = prompts.busy_prompt.clone() {
                                prompts.comfort_prompts.push(crate::call::ComfortPrompt {
                                    audio_file: busy,
                                    interval_secs: 30,
                                });
                            }
                        }
                        config.voice_prompts = Some(prompts.clone());
                    }
                }

                let mut app = crate::call::app::queue::QueueApp::new(plan, config)
                    .with_call_id(context.call_info.session_id.clone());
                // `queue_joined` was already broadcast before agent resolution
                // (strict ordering) — suppress the duplicate in `on_enter`.
                if pending.joined_emitted {
                    app = app.with_joined_emitted_externally();
                }
                // Dynamic dial-list re-resolution (wait-retention assignment /
                // `resolve_agents`) must re-apply the enricher, or those
                // paths deliver the agent INVITE without the screen-pop
                // headers (prod: IVR → queue → queued → assigned = no
                // `Call-Info` / `User-to-User`).
                if let Some(setup) = pending.queue_enricher {
                    app = app.with_location_enricher(setup);
                }
                // The primary skill-group id must be visible to post-call
                // hooks (CSAT/wrapup/hold-music) even when no agent registry
                // is attached — QueueApp writes it to CallMeta via SipSession.
                if let Some(ref sg) = pending.skill_group_id {
                    app = app.with_skill_group(sg.clone());
                }
                if let Some(ref registry) = self.agent_registry {
                    app = app.with_agent_registry(registry.clone());
                    // Skill-group queue: pull the escalation plan (widening
                    // groups + thresholds + fair ordering) from the addon so
                    // the queue app can escalate after the configured wait.
                    if let Some(sg) = pending.skill_group_id.clone() {
                        let mut escalation = registry
                            .escalation_plan_for(&format!("skill-group:{}", sg))
                            .await;
                        // URI overflow params win over ACD policy and the
                        // skill-group's `overflow_groups` (partial override).
                        if let Some(ovr) = pending.overflow_overrides.as_ref() {
                            ovr.apply_to_plan(&mut escalation);
                        }
                        app = app.with_escalation_plan(escalation, sg);
                    }
                }
                Some(Box::new(app) as Box<dyn crate::call::app::CallApp>)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::app::testing::MockCallStack;
    use crate::call::{QueueHoldConfig, QueuePlan, VoicePrompts};

    /// The queue factory MUST copy `pending.plan.priority` into the app's
    /// `QueueConfig` — every downstream consumer (fifo gate registration,
    /// inline-ACD enqueue, shared-queue row) derives from it, and a reverted
    /// copy line silently re-hardcodes priority 0. Observable end-to-end via
    /// the app's own `queue_joined` emission on entry (`joined_emitted` =
    /// false here, so on_enter re-emits with the config's priority).
    #[tokio::test]
    async fn queue_factory_carries_plan_priority_into_the_app() {
        let gateway: crate::rwi::RwiGatewayRef =
            Arc::new(parking_lot::RwLock::new(crate::rwi::RwiGateway::new()));
        let mut tap = gateway.read().subscribe_events();

        let config: crate::config::Config = toml::from_str("[proxy]\naddr = \"0.0.0.0\"").unwrap();
        let mut ctx = ApplicationContext::new(
            sea_orm::DatabaseConnection::default(),
            CallInfo {
                session_id: "queue-prio-factory".into(),
                caller: "1001".into(),
                callee: "1002".into(),
                direction: "inbound".into(),
                started_at: chrono::Utc::now(),
                sip_headers: Default::default(),
                route_name: None,
            },
            Arc::new(config),
            reqwest::Client::new(),
        );
        ctx.rwi_gateway = Some(gateway.clone());
        *ctx.pending_queue.lock() = Some(PendingQueuePlan {
            plan: QueuePlan {
                queue_name: "support".into(),
                accept_immediately: true,
                dial_strategy: Some(DialStrategy::Sequential(vec![crate::call::Location {
                    aor: "sip:agent@example.com".parse().unwrap(),
                    ..Default::default()
                }])),
                priority: 7,
                ..Default::default()
            },
            agent_uris: vec!["sip:agent@example.com".into()],
            parallel: false,
            skill_group_id: None,
            overflow_overrides: None,
            joined_emitted: false,
            queue_enricher: None,
        });

        let app = BuiltinAppFactory::new(None, None)
            .create_app("queue", None, &ctx)
            .await
            .unwrap()
            .unwrap();
        let mut stack = MockCallStack::run_with_context(app, ctx);
        stack
            .assert_cmd(2000, "answer", |c| matches!(c, CallCommand::Answer { .. }))
            .await;

        // The on_enter `queue_joined` must carry the plan's priority.
        let joined = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match tap.recv().await {
                    Ok(entry) if entry.event.event_type == "queue_joined" => break entry,
                    Ok(_) => continue,
                    Err(_) => panic!("event tap closed before queue_joined"),
                }
            }
        })
        .await
        .expect("queue_joined must be emitted on entry");
        assert_eq!(joined.event.payload["queue_id"], "support");
        assert_eq!(
            joined.event.payload["priority"], 7,
            "factory must propagate plan.priority into the queue app"
        );

        stack.cancel();
        let _ = stack.join().await;
    }

    #[tokio::test]
    async fn queue_audio_prompt_policies_preserve_wait_music() {
        for enabled in [false, true] {
            let wait_policies: &[(Option<bool>, Option<bool>)] = if enabled {
                &[(None, None)]
            } else {
                &[
                    (Some(false), Some(false)),
                    (Some(false), Some(true)),
                    (Some(true), Some(false)),
                    (None, None),
                ]
            };
            for &(comfort, busy) in wait_policies {
                for language in ["default", "zh", "en"] {
                    let comfort_setting = comfort
                        .map(|value| format!("queue_comfort_prompt_enabled = {value}\n"))
                        .unwrap_or_default();
                    let busy_setting = busy
                        .map(|value| format!("queue_busy_prompt_enabled = {value}\n"))
                        .unwrap_or_default();
                    let config: crate::config::Config = toml::from_str(&format!(
                        "[proxy]\naddr = \"0.0.0.0\"\nqueue_transfer_prompt_enabled = {enabled}\n{comfort_setting}{busy_setting}"
                    ))
                    .unwrap();
                    let ctx = ApplicationContext::new(
                        sea_orm::DatabaseConnection::default(),
                        CallInfo {
                            session_id: "queue-audio-test".into(),
                            caller: "1001".into(),
                            callee: "1002".into(),
                            direction: "inbound".into(),
                            started_at: chrono::Utc::now(),
                            sip_headers: Default::default(),
                            route_name: None,
                        },
                        Arc::new(config),
                        reqwest::Client::new(),
                    );
                    *ctx.pending_queue.lock() = Some(PendingQueuePlan {
                        plan: QueuePlan {
                            queue_name: "support".into(),
                            accept_immediately: true,
                            dial_strategy: Some(DialStrategy::Parallel(vec![crate::call::Location {
                                aor: "sip:agent@example.com".parse().unwrap(),
                                ..Default::default()
                            }])),
                            hold: Some(
                                QueueHoldConfig::default().with_audio_file("sounds/wait.wav".into()),
                            ),
                            voice_prompts: match language {
                                "zh" => Some(VoicePrompts::zh()),
                                "en" => Some(VoicePrompts::en()),
                                _ => None,
                            },
                            ..Default::default()
                        },
                        agent_uris: vec!["sip:agent@example.com".into()],
                        parallel: true,
                        skill_group_id: Some("support".into()),
                        overflow_overrides: None,
                        joined_emitted: false,
                        queue_enricher: None,
                    });
                    let app = BuiltinAppFactory::new(None, None)
                        .create_app("queue", None, &ctx)
                        .await
                        .unwrap()
                        .unwrap();
                    let mut stack = MockCallStack::run_with_context(app, ctx);
                    stack
                        .assert_cmd(2000, "answer", |c| {
                            matches!(c, CallCommand::Answer { .. })
                        })
                        .await;
                    let music = stack.next_cmd(2000).await.unwrap();
                    let track_id = match music {
                        CallCommand::Play {
                            source: MediaSource::File { path },
                            options: Some(options),
                            ..
                        } => {
                            assert_eq!(path, "sounds/wait.wav");
                            options.track_id.unwrap()
                        }
                        other => panic!("expected wait music, got {other:?}"),
                    };
                    stack
                        .assert_cmd(2000, "dial agent", |c| {
                            matches!(c, CallCommand::LegAdd { .. })
                        })
                        .await;
                    if enabled {
                        let prompt = stack.next_cmd(2000).await.unwrap();
                        let suffix = if language == "en" { "en" } else { "zh" };
                        assert!(matches!(prompt, CallCommand::Play {
                            source: MediaSource::File { path }, ..
                        } if path == format!("sounds/queue-transfer-{suffix}.wav")));
                    } else {
                        assert!(
                            stack.next_cmd(100).await.is_none(),
                            "disabled {language} transfer prompt must not interrupt wait music"
                        );
                        stack.audio_complete(track_id);
                        if comfort != Some(false) {
                            let comfort = stack.next_cmd(2000).await.unwrap();
                            let suffix = if language == "en" { "en" } else { "zh" };
                            let comfort_track = match comfort {
                                CallCommand::Play {
                                    source: MediaSource::File { path },
                                    options: Some(options),
                                    ..
                                } => {
                                    assert_eq!(path, format!("sounds/queue-busy-{suffix}.wav"));
                                    options.track_id.unwrap()
                                }
                                other => panic!("expected comfort prompt, got {other:?}"),
                            };
                            stack.audio_complete(comfort_track);
                        }
                        stack
                            .assert_cmd(2000, "continue wait music", |c| {
                                matches!(c, CallCommand::Play {
                                    source: MediaSource::File { path }, ..
                                } if path == "sounds/wait.wav")
                            })
                            .await;
                        stack.custom("all_agents_busy", serde_json::json!({}));
                        if busy != Some(false) {
                            let busy_prompt = stack.next_cmd(2000).await.unwrap();
                            let suffix = if language == "en" { "en" } else { "zh" };
                            let busy_track = match busy_prompt {
                                CallCommand::Play {
                                    source: MediaSource::File { path },
                                    options: Some(options),
                                    ..
                                } => {
                                    assert_eq!(path, format!("sounds/queue-busy-{suffix}.wav"));
                                    options.track_id.unwrap()
                                }
                                other => panic!("expected busy prompt, got {other:?}"),
                            };
                            stack.audio_complete(busy_track);
                        }
                        stack
                            .assert_cmd(2000, "busy fallback must still hang up", |c| {
                                matches!(c, CallCommand::Hangup(_))
                            })
                            .await;
                    }
                    if enabled {
                        stack.remote_hangup();
                    }
                    stack.join().await.unwrap();
                }
            }
        }
    }
}
