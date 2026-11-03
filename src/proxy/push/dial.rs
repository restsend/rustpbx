//! INVITE delivery towards push devices (RFC 8599 §5.6.2).
//!
//! Every outgoing INVITE goes through [`send_invite`]. When it is addressed to
//! a push device, the device is always pushed, at once, whatever the state of
//! its flow. If the flow is up (or UDP), the INVITE goes out on it at the same
//! time. If the flow is down (the app was suspended or killed), the INVITE is
//! held — the RFC's "SIP Request Push Bucket" — until the device sends its
//! binding-refresh REGISTER. Each time the device registers on a new flow
//! before it replies to the INVITE, the INVITE is sent again there, with the
//! same Call-ID. Whoever built the INVITE (direct call, route, queue,
//! transfer, ...) needs no knowledge of push.

use super::{
    FlowState, PushDevice, PushError, PushKind, PushNotification, PushParams, PushService,
};
use crate::call::Location;
use crate::proxy::locator::{Locator, LocatorEvent};
use crate::proxy::server::SipServerRef;
use rsipstack::dialog::DialogId;
use rsipstack::dialog::dialog::{DialogState, DialogStateSender};
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::invite_dialog::InviteDialog;
use rsipstack::sip::{Response, StatusCode, Uri};
use rsipstack::transport::SipAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, info, warn};

pub type InviteOutcome = rsipstack::Result<(InviteDialog, Option<Response>)>;

/// Send an INVITE like [`DialogLayer::do_invite`], waking the callee first if
/// it is a sleeping push device. Returns the outcome and the INVITE actually
/// sent: when the device re-registered, its callee and destination follow the
/// device's fresh binding.
pub async fn send_invite(
    server: SipServerRef,
    option: InviteOption,
    state_tx: DialogStateSender,
) -> (InviteOutcome, InviteOption) {
    let push = server
        .push
        .clone()
        .and_then(|service| PushDial::for_invite(&server, service, &option));
    invite_with_push(server.dialog_layer.clone(), option, state_tx, push).await
}

/// How an INVITE to a push device is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The device's flow is down: hold the INVITE until it re-registers.
    AwaitRegistration,
    /// The flow looks alive (or is UDP): send the INVITE now. The device is
    /// pushed too: a dozing phone can sit on a half-open flow. If the device
    /// re-registers on another flow before responding, the INVITE moves
    /// there.
    DialAndPush,
}

/// Push handling for one INVITE.
struct PushDial {
    service: Arc<PushService>,
    device: PushDevice,
    mode: Mode,
    watch: RegistrationWatch,
    push_result: Option<oneshot::Receiver<Result<(), PushError>>>,
    pushed_at: Instant,
}

impl PushDial {
    /// Start push handling if `option` is addressed to a push device:
    /// subscribe to registrations first (so a fast wake-up cannot be missed),
    /// then push the device in the background.
    fn for_invite(
        server: &SipServerRef,
        service: Arc<PushService>,
        option: &InviteOption,
    ) -> Option<Self> {
        let device = service.device_for_invite(&option.callee, option.destination.as_ref())?;
        let mode = match option.destination.as_ref().map(|d| service.flow_state(d)) {
            Some(FlowState::Down) => Mode::AwaitRegistration,
            _ => Mode::DialAndPush,
        };
        let watch = RegistrationWatch {
            rx: server.locator_events.as_ref()?.subscribe(),
            locator: server.locator.clone(),
            service: service.clone(),
            device: device.clone(),
            since: Instant::now(),
        };

        let include_caller = service.config().include_caller_info;
        let notification = PushNotification {
            kind: PushKind::Call,
            call_id: option.call_id.clone(),
            caller_uri: include_caller.then(|| option.caller.to_string()),
            caller_name: option
                .caller_display_name
                .clone()
                .filter(|_| include_caller),
            ttl: service.wakeup_timeout(),
        };
        info!(
            identity = %device.identity,
            contact = %device.contact,
            ?mode,
            call_id = ?option.call_id,
            "pushing callee device"
        );
        let (tx, rx) = oneshot::channel();
        let sender = service.clone();
        let pushed = device.clone();
        crate::utils::spawn(async move {
            let _ = tx.send(sender.send(&pushed, &notification).await);
        });

        Some(Self {
            service,
            device,
            mode,
            watch,
            push_result: Some(rx),
            pushed_at: Instant::now(),
        })
    }

    /// Wait for the device's binding-refresh REGISTER, at most the wake-up
    /// timeout after the push. Gives up early when the push itself failed
    /// (RFC 8599 §5.6.2).
    async fn await_registration(&mut self) -> Option<Location> {
        let Self {
            service,
            device,
            watch,
            push_result,
            pushed_at,
            ..
        } = self;
        // Bounded from the push, so a later wait (after a failed INVITE) only
        // gets the remaining time.
        let deadline =
            tokio::time::sleep(service.wakeup_timeout().saturating_sub(pushed_at.elapsed()));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                fresh = watch.next() => {
                    if let Some(location) = &fresh {
                        let elapsed = pushed_at.elapsed();
                        crate::metrics::push::wakeup("registered");
                        crate::metrics::push::wakeup_latency_seconds(elapsed.as_secs_f64());
                        info!(
                            identity = %device.identity,
                            contact = %location.aor,
                            elapsed_ms = elapsed.as_millis() as u64,
                            "pushed device re-registered"
                        );
                    }
                    return fresh;
                }
                result = async { push_result.as_mut().expect("guarded").await }, if push_result.is_some() => {
                    *push_result = None;
                    if let Ok(Err(error)) = result {
                        crate::metrics::push::wakeup("push_failed");
                        warn!(identity = %device.identity, %error, "push failed; not waiting for the device");
                        return None;
                    }
                }
                _ = &mut deadline => {
                    crate::metrics::push::wakeup("timeout");
                    warn!(
                        identity = %device.identity,
                        timeout_secs = service.wakeup_timeout().as_secs(),
                        "pushed device did not re-register in time"
                    );
                    return None;
                }
            }
        }
    }

    /// Resolves with the device's new binding when it re-registers on a flow
    /// other than `current` before the INVITE got any response.
    async fn moved(
        &mut self,
        current: Option<&SipAddr>,
        responded: &AtomicBool,
    ) -> Option<Location> {
        loop {
            let fresh = self.watch.next().await?;
            if responded.load(Ordering::Acquire) || fresh.destination.as_ref() == current {
                continue;
            }
            return Some(fresh);
        }
    }
}

/// Follows locator registrations of one device.
struct RegistrationWatch {
    rx: broadcast::Receiver<LocatorEvent>,
    locator: Arc<Box<dyn Locator>>,
    service: Arc<PushService>,
    device: PushDevice,
    since: Instant,
}

impl RegistrationWatch {
    /// Next binding registered by the device; `None` once the event channel
    /// is closed (shutdown).
    async fn next(&mut self) -> Option<Location> {
        loop {
            match self.rx.recv().await {
                // A sleeping device still has its binding: its REGISTER is
                // a refresh of its identity.
                Ok(LocatorEvent::Registered(location) | LocatorEvent::Refreshed(location)) => {
                    if self.matches(&location).await {
                        return Some(location);
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    debug!(missed, "registration events lagged; checking the locator");
                    if let Some(location) = self
                        .find_binding(|l| l.last_modified.is_some_and(|t| t >= self.since))
                        .await
                    {
                        return Some(location);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }

    /// A binding of the device on a flow that is up right now — it came back
    /// on its own since the INVITE was built.
    async fn live_binding(&self) -> Option<Location> {
        self.find_binding(|l| {
            l.destination
                .as_ref()
                .is_some_and(|d| self.service.flow_state(d) == FlowState::Up)
        })
        .await
    }

    async fn matches(&self, location: &Location) -> bool {
        if location.expires == 0 || location.destination.is_none() {
            return false;
        }
        if !self.device.same_device(
            location.instance_id.as_deref(),
            PushParams::from_uri(&location.aor).as_ref(),
        ) {
            return false;
        }
        let Some(username) = location.registered_username.as_deref() else {
            return false;
        };
        self.locator
            .get_identifier(username, location.registered_realm.as_deref())
            .await
            == self.device.identity
    }

    async fn find_binding(&self, filter: impl Fn(&Location) -> bool) -> Option<Location> {
        let aor = Uri::try_from(self.device.aor.as_str()).ok()?;
        let now = Instant::now();
        for location in self.locator.lookup(&aor).await.ok()? {
            if !location.is_expired_at(now) && filter(&location) && self.matches(&location).await {
                return Some(location);
            }
        }
        None
    }
}

const WAKEUP_FAILED: &str = "Pushed device did not re-register";

async fn invite_with_push(
    dialog_layer: Arc<DialogLayer>,
    mut option: InviteOption,
    state_tx: DialogStateSender,
    push: Option<PushDial>,
) -> (InviteOutcome, InviteOption) {
    let Some(mut push) = push else {
        let result = dialog_layer.do_invite(option.clone(), state_tx).await;
        return (result, option);
    };

    if push.mode == Mode::AwaitRegistration {
        // The device may have come back on its own since the INVITE was
        // built; it would not re-register again for the push.
        let fresh = match push.watch.live_binding().await {
            Some(live) => Some(live),
            None => push.await_registration().await,
        };
        match fresh {
            Some(fresh) => retarget(&mut option, &fresh),
            None => return (Err(wakeup_failed_error(&option)), option),
        }
    }

    loop {
        let responded = Arc::new(AtomicBool::new(false));
        let tracked_tx = track_responses(&dialog_layer, state_tx.clone(), responded.clone());
        let current = option.destination.clone();
        let invite = dialog_layer.do_invite(option.clone(), tracked_tx);
        tokio::pin!(invite);

        // Without a destination the INVITE is routed by URI (e.g. via a
        // cluster home proxy); there is no flow to follow.
        if current.is_none() {
            return (invite.await, option);
        }
        let result = tokio::select! {
            result = &mut invite => result,
            fresh = push.moved(current.as_ref(), &responded) => match fresh {
                Some(fresh) => {
                    info!(
                        identity = %push.device.identity,
                        from = ?current,
                        to = ?fresh.destination,
                        "device re-registered on a new flow before answering; moving the INVITE there"
                    );
                    // Dropping the unanswered INVITE abandons it (rsipstack
                    // CANCELs it if a provisional response ever shows up).
                    retarget(&mut option, &fresh);
                    continue;
                }
                None => invite.await,
            },
        };
        // The flow failed before the device said anything: it is presumably
        // waking up from the push, so wait for it to re-register.
        if !responded.load(Ordering::Acquire) && flow_failed(&result) {
            debug!(
                identity = %push.device.identity,
                "INVITE to the device's last flow failed; waiting for it to re-register"
            );
            if let Some(fresh) = push.await_registration().await {
                retarget(&mut option, &fresh);
                continue;
            }
        }
        return (result, option);
    }
}

/// Whether an INVITE ended without reaching the UA: a transport error, no
/// response, or a locally generated timeout / unavailable response.
fn flow_failed(result: &InviteOutcome) -> bool {
    match result {
        Err(_) | Ok((_, None)) => true,
        Ok((_, Some(response))) => matches!(response.status_code.code(), 408 | 503),
    }
}

/// Point the INVITE at the device's fresh binding.
fn retarget(option: &mut InviteOption, fresh: &Location) {
    option.callee = fresh.aor.clone();
    option.destination = fresh.destination.clone();
}

/// Relay dialog states to `outer`, flagging `responded` once the INVITE got
/// any response.
fn track_responses(
    dialog_layer: &DialogLayer,
    outer: DialogStateSender,
    responded: Arc<AtomicBool>,
) -> DialogStateSender {
    let (tx, mut rx) = dialog_layer.new_dialog_state_channel();
    crate::utils::spawn(async move {
        while let Some(state) = rx.recv().await {
            if matches!(
                state,
                DialogState::Trying(_) | DialogState::Early(..) | DialogState::Confirmed(..)
            ) {
                responded.store(true, Ordering::Release);
            }
            if outer.send(state).is_err() {
                break;
            }
        }
    });
    tx
}

fn wakeup_failed_error(option: &InviteOption) -> rsipstack::Error {
    rsipstack::Error::DialogError(
        WAKEUP_FAILED.to_string(),
        DialogId {
            call_id: option.call_id.clone().unwrap_or_default(),
            local_tag: String::new(),
            remote_tag: String::new(),
        },
        StatusCode::TemporarilyUnavailable,
    )
}

/// Status for an INVITE that failed because the pushed device never
/// re-registered (RFC 8599 §5.6.2: 480 Temporarily Unavailable).
pub fn wakeup_failure_status(error: &rsipstack::Error) -> Option<(StatusCode, String)> {
    match error {
        rsipstack::Error::DialogError(reason, _, code) if reason == WAKEUP_FAILED => {
            Some((code.clone(), reason.clone()))
        }
        _ => None,
    }
}
