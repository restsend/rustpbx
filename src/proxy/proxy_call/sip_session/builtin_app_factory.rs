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
            "ivr" => {
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

                    let mut provider = crate::call::app::ivr::StepProvider::new(url);

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
                        let mut provider =
                            crate::call::app::ivr::StepProvider::new(&provider_cfg.url);
                        for (k, v) in &provider_cfg.headers {
                            provider.add_header(k, v);
                        }
                        provider = provider
                            .with_retry(crate::call::app::ivr::RetryConfig::from(provider_cfg));
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
                let mut app = crate::call::app::voicemail::VoicemailApp::new(extension);
                if let Some(greeting) = params
                    .as_ref()?
                    .get("greeting_path")
                    .and_then(|v| v.as_str())
                {
                    app = app.with_greeting_path(greeting);
                }
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
                config.name = plan.queue_name.clone();
                config.accept_immediately = plan.accept_immediately;
                config.hold = plan.hold.clone();
                config.fallback = plan.fallback.clone();
                config.voice_prompts = plan.voice_prompts.clone();
                config.ring_timeout = plan.ring_timeout;
                if let Some(ref label) = plan.label {
                    if !config.name.is_empty() {
                        config.name = label.clone();
                    } else {
                        config.name = label.clone();
                    }
                }
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
