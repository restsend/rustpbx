use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc, Weekday};
use chrono_tz::Tz;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Interactive Voice Response plan definition
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IvrPlan {
    pub id: String,
    #[serde(default)]
    pub version: u32,
    pub entry_step: String,
    pub steps: HashMap<String, IvrStep>,
    #[serde(default)]
    pub calendars: HashMap<String, WorkingCalendar>,
}

impl IvrPlan {
    pub fn validate(&self) -> Result<()> {
        if !self.steps.contains_key(&self.entry_step) {
            bail!(
                "entry step '{}' not found in plan {}",
                self.entry_step,
                self.id
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IvrStep {
    Prompt(PromptStep),
    Input(InputStep),
    Branch(BranchStep),
    Action(ActionStep),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptStep {
    #[serde(default)]
    pub prompts: Vec<PromptMedia>,
    #[serde(default)]
    pub allow_barge_in: bool,
    pub next: Option<String>,
    #[serde(default)]
    pub availability: Option<StepAvailability>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptMedia {
    pub source: PromptSource,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub loop_count: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PromptSource {
    File { file: PromptFile },
    Tts { tts: PromptTts },
    Url { url: PromptUrl },
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptFile {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptTts {
    pub text: String,
    #[serde(default)]
    pub voice: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PromptUrl {
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct InputStep {
    pub min_digits: usize,
    #[serde(default)]
    pub max_digits: Option<usize>,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default = "default_attempt_limit")]
    pub attempt_limit: Option<u32>,
    #[serde(default)]
    pub allow_barge_in: bool,
    pub on_valid: StepTarget,
    #[serde(default)]
    pub on_invalid: Option<StepTarget>,
    #[serde(default)]
    pub on_timeout: Option<StepTarget>,
    #[serde(default)]
    pub on_error: Option<StepTarget>,
    #[serde(default)]
    pub availability: Option<StepAvailability>,
}

const DEFAULT_ATTEMPTS: u32 = 3;

fn default_attempt_limit() -> Option<u32> {
    Some(DEFAULT_ATTEMPTS)
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct StepTarget {
    #[serde(default)]
    pub goto: Option<String>,
    #[serde(default)]
    pub action: Option<IvrAction>,
    #[serde(default)]
    pub hangup: Option<HangupAction>,
    #[serde(default)]
    pub end: Option<bool>,
}

impl StepTarget {
    pub(crate) fn resolve(&self) -> Result<Transition> {
        if let Some(action) = &self.action {
            return Ok(Transition::Action(action.clone()));
        }
        if let Some(goto) = &self.goto {
            return Ok(Transition::Step(goto.clone()));
        }
        if let Some(hangup) = &self.hangup {
            return Ok(Transition::Hangup(hangup.clone()));
        }
        if self.end.unwrap_or(false) {
            return Ok(Transition::End);
        }
        bail!("step target missing goto/action/end definition")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct StepAvailability {
    pub calendar: String,
    pub when_closed: StepTarget,
    #[serde(default)]
    pub allow_override: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct WorkingCalendar {
    pub timezone: String,
    #[serde(default)]
    pub weekly: Vec<WeeklyWindow>,
    #[serde(default)]
    pub overrides: Vec<OverrideWindow>,
    #[serde(default)]
    pub closed: Vec<ClosedDate>,
}

impl WorkingCalendar {
    pub fn is_open(&self, now_utc: DateTime<Utc>) -> Result<bool> {
        if self.weekly.is_empty() && self.overrides.is_empty() && self.closed.is_empty() {
            return Ok(true);
        }

        let tz: Tz = self
            .timezone
            .parse()
            .map_err(|_| anyhow!("invalid timezone '{}'", self.timezone))?;
        let local = now_utc.with_timezone(&tz);
        let date = local.date_naive();
        let time = local.time();

        if self.is_closed_date(date)? {
            return Ok(false);
        }

        if let Some(open) = self.evaluate_overrides(date, time)? {
            return Ok(open);
        }

        if self.weekly.is_empty() {
            return Ok(true);
        }

        let weekday = local.weekday();
        for window in &self.weekly {
            if window.matches_weekday(weekday)? {
                if window.contains_time(time)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn is_closed_date(&self, date: NaiveDate) -> Result<bool> {
        for entry in &self.closed {
            if parse_date(&entry.date)? == date {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn evaluate_overrides(&self, date: NaiveDate, time: NaiveTime) -> Result<Option<bool>> {
        let mut matched = false;
        for window in &self.overrides {
            if parse_date(&window.date)? == date {
                matched = true;
                if window.contains_time(time)? {
                    return Ok(Some(true));
                }
            }
        }
        if matched {
            return Ok(Some(false));
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct WeeklyWindow {
    pub days: Vec<String>,
    pub start: String,
    pub end: String,
}

impl WeeklyWindow {
    fn matches_weekday(&self, weekday: Weekday) -> Result<bool> {
        if self.days.is_empty() {
            return Ok(true);
        }
        for day in &self.days {
            if parse_weekday(day)? == weekday {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn contains_time(&self, time: NaiveTime) -> Result<bool> {
        let start = parse_time(&self.start)?;
        let end = parse_time(&self.end)?;
        Ok(time_in_window(time, start, end))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct OverrideWindow {
    pub date: String,
    pub start: String,
    pub end: String,
}

impl OverrideWindow {
    fn contains_time(&self, time: NaiveTime) -> Result<bool> {
        let start = parse_time(&self.start)?;
        let end = parse_time(&self.end)?;
        Ok(time_in_window(time, start, end))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClosedDate {
    pub date: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct BranchStep {
    pub branches: Vec<BranchRoute>,
    #[serde(default)]
    pub default: Option<StepTarget>,
    #[serde(default)]
    pub availability: Option<StepAvailability>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct BranchRoute {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub goto: Option<String>,
    #[serde(default)]
    pub action: Option<IvrAction>,
}

impl BranchRoute {
    fn matches(&self, digits: &str) -> Result<Option<Vec<String>>> {
        if let Some(value) = &self.value {
            if digits == value {
                return Ok(Some(vec![digits.to_string()]));
            }
        }
        if let Some(prefix) = &self.prefix {
            if digits.starts_with(prefix) {
                return Ok(Some(vec![digits.to_string()]));
            }
        }
        if let Some(pattern) = &self.regex {
            let regex = Regex::new(pattern)
                .map_err(|err| anyhow!("invalid branch regex '{}': {err}", pattern))?;
            if let Some(captures) = regex.captures(digits) {
                let mut values = Vec::new();
                for idx in 0..captures.len() {
                    values.push(
                        captures
                            .get(idx)
                            .map(|m| m.as_str())
                            .unwrap_or("")
                            .to_string(),
                    );
                }
                return Ok(Some(values));
            }
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ActionStep {
    pub action: IvrAction,
    #[serde(default)]
    pub availability: Option<StepAvailability>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum IvrAction {
    Transfer { transfer: TransferAction },
    Queue { queue: QueueAction },
    Webhook { webhook: WebhookAction },
    Playback { playback: PlaybackAction },
    Hangup { hangup: HangupAction },
}

impl IvrAction {
    pub fn transfer(config: TransferAction) -> Self {
        Self::Transfer { transfer: config }
    }

    pub fn queue(config: QueueAction) -> Self {
        Self::Queue { queue: config }
    }

    pub fn webhook(config: WebhookAction) -> Self {
        Self::Webhook { webhook: config }
    }

    pub fn playback(config: PlaybackAction) -> Self {
        Self::Playback { playback: config }
    }

    pub fn hangup(config: HangupAction) -> Self {
        Self::Hangup { hangup: config }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct TransferAction {
    pub target: String,
    #[serde(default)]
    pub caller_id: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct QueueAction {
    pub queue: String,
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub priority: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct WebhookAction {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlaybackAction {
    pub prompt: PromptMedia,
    #[serde(default)]
    pub auto_hangup: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
pub struct HangupAction {
    #[serde(default)]
    pub code: Option<u16>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub trait IvrRuntime {
    fn play_prompt(
        &mut self,
        plan: &IvrPlan,
        _step_id: &str,
        prompt: &PromptMedia,
        allow_barge_in: bool,
    ) -> Result<PromptPlayback>;

    fn collect_input(
        &mut self,
        plan: &IvrPlan,
        step_id: &str,
        input: &InputStep,
        attempt: u32,
    ) -> Result<InputEvent>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptPlayback {
    Completed,
    BargeIn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Digits(String),
    Timeout,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IvrExit {
    Completed,
    Transfer(ResolvedTransferAction),
    Queue(ResolvedQueueAction),
    Webhook(ResolvedWebhookAction),
    Playback(PlaybackAction),
    Hangup(HangupAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTransferAction {
    pub target: String,
    pub caller_id: Option<String>,
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedQueueAction {
    pub queue: String,
    pub strategy: Option<String>,
    pub priority: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWebhookAction {
    pub url: String,
    pub method: String,
    pub payload: Option<String>,
    pub headers: HashMap<String, String>,
}

pub struct IvrExecutor<'a, R: IvrRuntime> {
    plan: &'a IvrPlan,
    runtime: &'a mut R,
    max_steps: usize,
    now_utc: DateTime<Utc>,
    availability_override: bool,
}

impl<'a, R: IvrRuntime> IvrExecutor<'a, R> {
    pub fn new(plan: &'a IvrPlan, runtime: &'a mut R) -> Self {
        Self {
            plan,
            runtime,
            max_steps: 1024,
            now_utc: Utc::now(),
            availability_override: false,
        }
    }

    pub fn with_step_limit(mut self, limit: usize) -> Self {
        self.max_steps = limit;
        self
    }

    pub fn with_current_time(mut self, now: DateTime<Utc>) -> Self {
        self.now_utc = now;
        self
    }

    pub fn with_availability_override(mut self, enabled: bool) -> Self {
        self.availability_override = enabled;
        self
    }

    pub fn run(&mut self) -> Result<IvrExit> {
        self.plan.validate()?;
        let mut state = IvrSessionState::new(self.plan.entry_step.clone());
        for _ in 0..self.max_steps {
            let step_id = state.current_step.clone();
            let step = self
                .plan
                .steps
                .get(&step_id)
                .ok_or_else(|| anyhow!("step '{}' not found in plan", step_id))?;

            match step {
                IvrStep::Prompt(prompt) => {
                    if let Some(outcome) = self.enforce_availability(
                        &step_id,
                        prompt.availability.as_ref(),
                        &mut state,
                    )? {
                        match outcome {
                            StepOutcome::Continue => continue,
                            StepOutcome::Exit(result) => return Ok(result),
                        }
                    }
                    for prompt_media in &prompt.prompts {
                        self.runtime.play_prompt(
                            self.plan,
                            &step_id,
                            prompt_media,
                            prompt.allow_barge_in,
                        )?;
                    }
                    if let Some(next) = &prompt.next {
                        state.move_to(next);
                        continue;
                    }
                    return Ok(IvrExit::Completed);
                }
                IvrStep::Input(input) => {
                    if let Some(outcome) = self.enforce_availability(
                        &step_id,
                        input.availability.as_ref(),
                        &mut state,
                    )? {
                        match outcome {
                            StepOutcome::Continue => continue,
                            StepOutcome::Exit(result) => return Ok(result),
                        }
                    }
                    let exit = self.handle_input_step(&step_id, input, &mut state)?;
                    match exit {
                        StepOutcome::Continue => continue,
                        StepOutcome::Exit(result) => return Ok(result),
                    }
                }
                IvrStep::Branch(branch) => {
                    if let Some(outcome) = self.enforce_availability(
                        &step_id,
                        branch.availability.as_ref(),
                        &mut state,
                    )? {
                        match outcome {
                            StepOutcome::Continue => continue,
                            StepOutcome::Exit(result) => return Ok(result),
                        }
                    }
                    let exit = self.handle_branch_step(&step_id, branch, &mut state)?;
                    match exit {
                        StepOutcome::Continue => continue,
                        StepOutcome::Exit(result) => return Ok(result),
                    }
                }
                IvrStep::Action(action_step) => {
                    if let Some(outcome) = self.enforce_availability(
                        &step_id,
                        action_step.availability.as_ref(),
                        &mut state,
                    )? {
                        match outcome {
                            StepOutcome::Continue => continue,
                            StepOutcome::Exit(result) => return Ok(result),
                        }
                    }
                    if let Some(result) =
                        self.execute_action(&action_step.action, &state.context)?
                    {
                        return Ok(result);
                    }
                    return Ok(IvrExit::Completed);
                }
            }
        }
        bail!("ivr execution exceeded step limit")
    }

    fn handle_input_step(
        &mut self,
        step_id: &str,
        input: &InputStep,
        state: &mut IvrSessionState,
    ) -> Result<StepOutcome> {
        let max_attempts = input.attempt_limit.unwrap_or(DEFAULT_ATTEMPTS);
        loop {
            let attempt = state.increment_attempt(step_id);
            let event = self
                .runtime
                .collect_input(self.plan, step_id, input, attempt)?;
            match event {
                InputEvent::Digits(value) => {
                    if !self.validate_digit_length(&value, input)? {
                        if let Some(outcome) =
                            self.on_invalid(step_id, input, state, attempt, max_attempts)?
                        {
                            return Ok(outcome);
                        }
                        continue;
                    }

                    if let Some(regex) = &input.regex {
                        let compiled = Regex::new(regex)
                            .map_err(|err| anyhow!("invalid input regex '{}': {err}", regex))?;
                        if let Some(captures) = compiled.captures(&value) {
                            let capture_values = collect_captures(&captures);
                            state.set_input(value.clone(), capture_values);
                            let transition = input.on_valid.resolve()?;
                            if let Some(result) = self.apply_transition(transition, state)? {
                                return Ok(StepOutcome::Exit(result));
                            }
                            return Ok(StepOutcome::Continue);
                        }
                        if let Some(outcome) =
                            self.on_invalid(step_id, input, state, attempt, max_attempts)?
                        {
                            return Ok(outcome);
                        }
                        continue;
                    }

                    state.set_input(value.clone(), vec![value.clone()]);
                    let transition = input.on_valid.resolve()?;
                    if let Some(result) = self.apply_transition(transition, state)? {
                        return Ok(StepOutcome::Exit(result));
                    }
                    return Ok(StepOutcome::Continue);
                }
                InputEvent::Timeout => {
                    if let Some(target) = &input.on_timeout {
                        let transition = target.resolve()?;
                        if let Some(result) = self.apply_transition(transition, state)? {
                            return Ok(StepOutcome::Exit(result));
                        }
                        return Ok(StepOutcome::Continue);
                    }
                    if let Some(outcome) =
                        self.on_invalid(step_id, input, state, attempt, max_attempts)?
                    {
                        return Ok(outcome);
                    }
                }
                InputEvent::Cancel => {
                    return Ok(StepOutcome::Exit(IvrExit::Hangup(HangupAction {
                        code: Some(487),
                        reason: Some("caller_cancelled".to_string()),
                    })));
                }
            }
        }
    }

    fn on_invalid(
        &mut self,
        _step_id: &str,
        input: &InputStep,
        state: &mut IvrSessionState,
        attempt: u32,
        max_attempts: u32,
    ) -> Result<Option<StepOutcome>> {
        if let Some(target) = &input.on_invalid {
            let transition = target.resolve()?;
            if let Some(result) = self.apply_transition(transition, state)? {
                return Ok(Some(StepOutcome::Exit(result)));
            }
            return Ok(Some(StepOutcome::Continue));
        }
        if attempt >= max_attempts {
            if let Some(target) = &input.on_error {
                let transition = target.resolve()?;
                if let Some(result) = self.apply_transition(transition, state)? {
                    return Ok(Some(StepOutcome::Exit(result)));
                }
                return Ok(Some(StepOutcome::Continue));
            }
            return Ok(Some(StepOutcome::Exit(IvrExit::Hangup(HangupAction {
                code: Some(400),
                reason: Some(format!("invalid input after {attempt} attempts")),
            }))));
        }
        Ok(None)
    }

    fn validate_digit_length(&self, digits: &str, input: &InputStep) -> Result<bool> {
        if digits.len() < input.min_digits {
            return Ok(false);
        }
        if let Some(max) = input.max_digits {
            if digits.len() > max {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn handle_branch_step(
        &mut self,
        step_id: &str,
        branch: &BranchStep,
        state: &mut IvrSessionState,
    ) -> Result<StepOutcome> {
        let digits = state
            .context
            .digits
            .clone()
            .ok_or_else(|| anyhow!("branch step '{}' requires prior input", step_id))?;
        for route in &branch.branches {
            if let Some(captures) = route.matches(&digits)? {
                state.set_input(digits.clone(), captures);
                if let Some(action) = &route.action {
                    if let Some(result) = self.execute_action(action, &state.context)? {
                        return Ok(StepOutcome::Exit(result));
                    }
                    return Ok(StepOutcome::Continue);
                }
                if let Some(next) = &route.goto {
                    state.move_to(next);
                    return Ok(StepOutcome::Continue);
                }
                bail!("branch route missing action or goto");
            }
        }
        if let Some(target) = &branch.default {
            let transition = target.resolve()?;
            if let Some(result) = self.apply_transition(transition, state)? {
                return Ok(StepOutcome::Exit(result));
            }
            return Ok(StepOutcome::Continue);
        }
        bail!("no matching branch in step '{}'", step_id)
    }

    fn execute_action(
        &self,
        action: &IvrAction,
        context: &TemplateContext,
    ) -> Result<Option<IvrExit>> {
        let result = match action {
            IvrAction::Transfer { transfer } => Some(IvrExit::Transfer(ResolvedTransferAction {
                target: render_template(&transfer.target, context)?,
                caller_id: match &transfer.caller_id {
                    Some(value) => Some(render_template(value, context)?),
                    None => None,
                },
                headers: render_map(&transfer.headers, context)?,
            })),
            IvrAction::Queue { queue } => Some(IvrExit::Queue(ResolvedQueueAction {
                queue: queue.queue.clone(),
                strategy: queue.strategy.clone(),
                priority: queue.priority,
            })),
            IvrAction::Webhook { webhook } => Some(IvrExit::Webhook(ResolvedWebhookAction {
                url: webhook.url.clone(),
                method: webhook.method.clone().unwrap_or_else(|| "POST".to_string()),
                payload: match &webhook.payload {
                    Some(payload) => Some(render_template(payload, context)?),
                    None => None,
                },
                headers: render_map(&webhook.headers, context)?,
            })),
            IvrAction::Playback { playback } => Some(IvrExit::Playback(playback.clone())),
            IvrAction::Hangup { hangup } => Some(IvrExit::Hangup(hangup.clone())),
        };
        Ok(result)
    }

    fn apply_transition(
        &mut self,
        transition: Transition,
        state: &mut IvrSessionState,
    ) -> Result<Option<IvrExit>> {
        match transition {
            Transition::Step(next) => {
                state.move_to(&next);
                Ok(None)
            }
            Transition::Action(action) => self.execute_action(&action, &state.context),
            Transition::Hangup(hangup) => Ok(Some(IvrExit::Hangup(hangup))),
            Transition::End => Ok(Some(IvrExit::Completed)),
        }
    }

    fn enforce_availability(
        &mut self,
        step_id: &str,
        availability: Option<&StepAvailability>,
        state: &mut IvrSessionState,
    ) -> Result<Option<StepOutcome>> {
        let Some(config) = availability else {
            return Ok(None);
        };

        if self.availability_override && config.allow_override {
            return Ok(None);
        }

        let calendar = self.plan.calendars.get(&config.calendar).ok_or_else(|| {
            anyhow!(
                "calendar '{}' referenced by step '{}' not found",
                config.calendar,
                step_id
            )
        })?;

        if calendar.is_open(self.now_utc)? {
            return Ok(None);
        }

        let transition = config.when_closed.resolve()?;
        if let Some(result) = self.apply_transition(transition, state)? {
            return Ok(Some(StepOutcome::Exit(result)));
        }
        Ok(Some(StepOutcome::Continue))
    }
}

#[derive(Debug)]
enum StepOutcome {
    Continue,
    Exit(IvrExit),
}

#[derive(Debug)]
struct IvrSessionState {
    current_step: String,
    attempts: HashMap<String, u32>,
    context: TemplateContext,
}

impl IvrSessionState {
    fn new(entry: String) -> Self {
        Self {
            current_step: entry,
            attempts: HashMap::new(),
            context: TemplateContext::default(),
        }
    }

    fn move_to(&mut self, next: &str) {
        self.current_step = next.to_string();
    }

    fn increment_attempt(&mut self, step: &str) -> u32 {
        let counter = self.attempts.entry(step.to_string()).or_insert(0);
        *counter += 1;
        *counter
    }

    fn set_input(&mut self, digits: String, captures: Vec<String>) {
        self.context.digits = Some(digits);
        self.context.captures = captures;
    }
}

#[derive(Debug, Clone, Default)]
pub struct TemplateContext {
    pub digits: Option<String>,
    pub captures: Vec<String>,
}

fn parse_date(value: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|err| anyhow!("invalid date '{}': {}", value, err))
}

fn parse_time(value: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(value, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M"))
        .map_err(|err| anyhow!("invalid time '{}': {}", value, err))
}

fn parse_weekday(value: &str) -> Result<Weekday> {
    match value.to_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tues" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thur" | "thurs" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        other => Err(anyhow!("invalid weekday '{}': expected mon..sun", other)),
    }
}

fn time_in_window(time: NaiveTime, start: NaiveTime, end: NaiveTime) -> bool {
    if start <= end {
        time >= start && time < end
    } else {
        time >= start || time < end
    }
}

fn render_map(
    input: &HashMap<String, String>,
    ctx: &TemplateContext,
) -> Result<HashMap<String, String>> {
    let mut rendered = HashMap::new();
    for (key, value) in input {
        rendered.insert(key.clone(), render_template(value, ctx)?);
    }
    Ok(rendered)
}

fn render_template(template: &str, ctx: &TemplateContext) -> Result<String> {
    if !template.contains('{') {
        return Ok(template.to_string());
    }
    let mut result = String::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut index = String::new();
            let mut closed = false;
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == '}' {
                    closed = true;
                    break;
                }
                index.push(next);
            }
            if !closed {
                bail!(
                    "unclosed placeholder in template '{}': '{{{}'",
                    template,
                    index
                );
            }
            if index.is_empty() {
                if let Some(value) = ctx.digits.as_ref() {
                    result.push_str(value);
                }
                continue;
            }
            let idx: usize = index.parse().map_err(|err| {
                anyhow!(
                    "invalid capture placeholder '{{{}}}' in template '{}': {err}",
                    index,
                    template
                )
            })?;
            if let Some(value) = ctx.captures.get(idx) {
                result.push_str(value);
            } else if idx == 0 {
                if let Some(value) = ctx.digits.as_ref() {
                    result.push_str(value);
                }
            }
        } else {
            result.push(ch);
        }
    }
    Ok(result)
}

fn collect_captures(captures: &regex::Captures<'_>) -> Vec<String> {
    let mut values = Vec::new();
    for idx in 0..captures.len() {
        values.push(
            captures
                .get(idx)
                .map(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }
    values
}

#[derive(Debug, Clone)]
pub(crate) enum Transition {
    Step(String),
    Action(IvrAction),
    Hangup(HangupAction),
    End,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct MockRuntime {
        inputs: VecDeque<InputEvent>,
        prompts: Vec<String>,
    }

    impl MockRuntime {
        fn push_input(&mut self, event: InputEvent) {
            self.inputs.push_back(event);
        }
    }

    impl IvrRuntime for MockRuntime {
        fn play_prompt(
            &mut self,
            _plan: &IvrPlan,
            step_id: &str,
            _prompt: &PromptMedia,
            _allow_barge_in: bool,
        ) -> Result<PromptPlayback> {
            self.prompts.push(step_id.to_string());
            Ok(PromptPlayback::Completed)
        }

        fn collect_input(
            &mut self,
            _plan: &IvrPlan,
            step_id: &str,
            _input: &InputStep,
            _attempt: u32,
        ) -> Result<InputEvent> {
            self.prompts.push(format!("input:{}", step_id));
            self.inputs
                .pop_front()
                .ok_or_else(|| anyhow!("no input event queued"))
        }
    }

    fn prompt_source(text: &str) -> PromptMedia {
        PromptMedia {
            source: PromptSource::Tts {
                tts: PromptTts {
                    text: text.to_string(),
                    voice: None,
                },
            },
            locale: None,
            loop_count: None,
        }
    }

    fn build_plan() -> IvrPlan {
        let mut steps = HashMap::new();
        steps.insert(
            "welcome".to_string(),
            IvrStep::Prompt(PromptStep {
                prompts: vec![prompt_source("welcome")],
                allow_barge_in: true,
                next: Some("collect".to_string()),
                availability: None,
            }),
        );

        steps.insert(
            "collect".to_string(),
            IvrStep::Input(InputStep {
                min_digits: 1,
                max_digits: Some(4),
                regex: Some("^(0|[1-9][0-9]{2,3})$".to_string()),
                timeout_ms: Some(5_000),
                attempt_limit: Some(3),
                allow_barge_in: true,
                on_valid: StepTarget {
                    goto: Some("branch".to_string()),
                    ..Default::default()
                },
                on_invalid: Some(StepTarget {
                    goto: Some("invalid".to_string()),
                    ..Default::default()
                }),
                on_timeout: Some(StepTarget {
                    goto: Some("timeout".to_string()),
                    ..Default::default()
                }),
                on_error: Some(StepTarget {
                    hangup: Some(HangupAction {
                        code: Some(480),
                        reason: Some("too_many_errors".to_string()),
                    }),
                    ..Default::default()
                }),
                availability: None,
            }),
        );

        steps.insert(
            "invalid".to_string(),
            IvrStep::Prompt(PromptStep {
                prompts: vec![prompt_source("invalid")],
                allow_barge_in: false,
                next: Some("collect".to_string()),
                availability: None,
            }),
        );

        steps.insert(
            "timeout".to_string(),
            IvrStep::Action(ActionStep {
                action: IvrAction::hangup(HangupAction {
                    code: Some(408),
                    reason: Some("timeout".to_string()),
                }),
                availability: None,
            }),
        );

        steps.insert(
            "branch".to_string(),
            IvrStep::Branch(BranchStep {
                branches: vec![
                    BranchRoute {
                        value: Some("0".to_string()),
                        goto: Some("directory".to_string()),
                        ..Default::default()
                    },
                    BranchRoute {
                        regex: Some("^([1-9][0-9]{2,3})$".to_string()),
                        action: Some(IvrAction::transfer(TransferAction {
                            target: "sip:{1}@pbx.local".to_string(),
                            ..Default::default()
                        })),
                        ..Default::default()
                    },
                ],
                default: Some(StepTarget {
                    hangup: Some(HangupAction {
                        code: Some(404),
                        reason: Some("no_branch".to_string()),
                    }),
                    ..Default::default()
                }),
                availability: None,
            }),
        );

        steps.insert(
            "directory".to_string(),
            IvrStep::Action(ActionStep {
                action: IvrAction::webhook(WebhookAction {
                    url: "https://rustpbx.com/directory".to_string(),
                    payload: Some("{0}".to_string()),
                    ..Default::default()
                }),
                availability: None,
            }),
        );

        IvrPlan {
            id: "main".to_string(),
            version: 1,
            entry_step: "welcome".to_string(),
            steps,
            calendars: HashMap::new(),
        }
    }

    fn add_workday_calendar(plan: &mut IvrPlan) {
        plan.calendars.insert(
            "workday".to_string(),
            WorkingCalendar {
                timezone: "UTC".to_string(),
                weekly: vec![WeeklyWindow {
                    days: vec![
                        "mon".into(),
                        "tue".into(),
                        "wed".into(),
                        "thu".into(),
                        "fri".into(),
                    ],
                    start: "09:00".to_string(),
                    end: "18:00".to_string(),
                }],
                overrides: vec![],
                closed: vec![],
            },
        );

        plan.steps.insert(
            "after_hours".to_string(),
            IvrStep::Action(ActionStep {
                action: IvrAction::hangup(HangupAction {
                    code: Some(486),
                    reason: Some("after_hours".to_string()),
                }),
                availability: None,
            }),
        );

        if let Some(IvrStep::Branch(branch)) = plan.steps.get_mut("branch") {
            branch.availability = Some(StepAvailability {
                calendar: "workday".to_string(),
                when_closed: StepTarget {
                    goto: Some("after_hours".to_string()),
                    ..Default::default()
                },
                allow_override: true,
            });
        }
    }

    #[test]
    fn test_transfer_branch() {
        let plan = build_plan();
        let mut runtime = MockRuntime::default();
        runtime.push_input(InputEvent::Digits("1234".to_string()));

        let mut executor = IvrExecutor::new(&plan, &mut runtime);
        let result = executor.run().expect("ivr run");
        match result {
            IvrExit::Transfer(resolved) => {
                assert_eq!(resolved.target, "sip:1234@pbx.local");
            }
            other => panic!("unexpected result: {:?}", other),
        }
        assert!(runtime.prompts.iter().any(|p| p == "welcome"));
    }

    #[test]
    fn test_invalid_retry_then_timeout() {
        let plan = build_plan();
        let mut runtime = MockRuntime::default();
        runtime.push_input(InputEvent::Digits("99999".to_string()));
        runtime.push_input(InputEvent::Timeout);

        let mut executor = IvrExecutor::new(&plan, &mut runtime);
        let result = executor.run().expect("ivr run");
        match result {
            IvrExit::Hangup(h) => {
                assert_eq!(h.code, Some(408));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_directory_route() {
        let plan = build_plan();
        let mut runtime = MockRuntime::default();
        runtime.push_input(InputEvent::Digits("0".to_string()));

        let mut executor = IvrExecutor::new(&plan, &mut runtime);
        let result = executor.run().expect("ivr run");
        match result {
            IvrExit::Webhook(webhook) => {
                assert_eq!(webhook.url, "https://rustpbx.com/directory");
                assert_eq!(webhook.payload, Some("0".to_string()));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_branch_after_hours_redirects() {
        let mut plan = build_plan();
        add_workday_calendar(&mut plan);

        let mut runtime = MockRuntime::default();
        runtime.push_input(InputEvent::Digits("1234".to_string()));

        let now = Utc.with_ymd_and_hms(2025, 1, 1, 2, 0, 0).unwrap();
        let mut executor = IvrExecutor::new(&plan, &mut runtime).with_current_time(now);
        let result = executor.run().expect("ivr run");
        match result {
            IvrExit::Hangup(h) => {
                assert_eq!(h.reason.as_deref(), Some("after_hours"));
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_branch_calendar_open_allows_transfer() {
        let mut plan = build_plan();
        add_workday_calendar(&mut plan);

        let mut runtime = MockRuntime::default();
        runtime.push_input(InputEvent::Digits("1234".to_string()));

        let now = Utc.with_ymd_and_hms(2025, 1, 1, 10, 0, 0).unwrap();
        let mut executor = IvrExecutor::new(&plan, &mut runtime).with_current_time(now);
        let result = executor.run().expect("ivr run");
        match result {
            IvrExit::Transfer(resolved) => {
                assert_eq!(resolved.target, "sip:1234@pbx.local");
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn test_availability_override_bypasses_schedule() {
        let mut plan = build_plan();
        add_workday_calendar(&mut plan);

        let mut runtime = MockRuntime::default();
        runtime.push_input(InputEvent::Digits("1234".to_string()));

        let now = Utc.with_ymd_and_hms(2025, 1, 1, 2, 0, 0).unwrap();
        let mut executor = IvrExecutor::new(&plan, &mut runtime)
            .with_current_time(now)
            .with_availability_override(true);
        let result = executor.run().expect("ivr run");
        match result {
            IvrExit::Transfer(resolved) => {
                assert_eq!(resolved.target, "sip:1234@pbx.local");
            }
            other => panic!("unexpected result: {:?}", other),
        }
    }
}
