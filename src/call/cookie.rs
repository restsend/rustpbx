use crate::call::SipUser;
use rsipstack::transaction::key::TransactionKey;
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

#[derive(Default)]
pub enum SpamResult {
    #[default]
    Nice,
    Spam,
    IpBlacklist,
    UaBlacklist,
}

impl SpamResult {
    fn is_spam(&self) -> bool {
        !matches!(self, SpamResult::Nice)
    }
}

#[derive(Default)]
struct TransactionCookieInner {
    user: Option<SipUser>,
    spam_result: SpamResult,
    max_duration: Option<Duration>,
    extensions: http::Extensions,
}
#[derive(Clone, Default)]
pub struct TransactionCookie {
    inner: Arc<RwLock<TransactionCookieInner>>,
}

impl From<&TransactionKey> for TransactionCookie {
    fn from(_key: &TransactionKey) -> Self {
        Self {
            inner: Arc::new(RwLock::new(TransactionCookieInner {
                user: None,
                spam_result: SpamResult::Nice,
                max_duration: None,
                extensions: http::Extensions::new(),
            })),
        }
    }
}

impl TransactionCookie {
    pub fn set_user(&self, user: SipUser) {
        if let Ok(mut inner) = self.inner.write() {
            inner.user = Some(user);
        }
    }
    pub fn get_user(&self) -> Option<SipUser> {
        self.inner
            .read()
            .map(|inner| inner.user.clone())
            .ok()
            .flatten()
    }

    pub fn mark_as_spam(&self, r: SpamResult) {
        self.inner
            .try_write()
            .map(|mut inner| {
                inner.spam_result = r;
            })
            .ok();
    }

    pub fn is_spam(&self) -> bool {
        self.inner
            .try_read()
            .map(|inner| inner.spam_result.is_spam())
            .ok()
            .unwrap_or_default()
    }

    pub fn set_max_duration(&self, duration: Duration) {
        self.inner
            .try_write()
            .map(|mut inner| {
                inner.max_duration = Some(duration);
            })
            .ok();
    }

    pub fn get_max_duration(&self) -> Option<Duration> {
        self.inner
            .try_read()
            .map(|inner| inner.max_duration)
            .ok()
            .flatten()
    }

    pub fn with_extensions<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&http::Extensions) -> R,
    {
        self.inner.read().ok().map(|inner| f(&inner.extensions))
    }

    pub fn with_extensions_mut<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut http::Extensions) -> R,
    {
        self.inner
            .write()
            .ok()
            .map(|mut inner| f(&mut inner.extensions))
    }

    pub fn insert_extension<T: Clone + Send + Sync + 'static>(&self, val: T) {
        self.with_extensions_mut(|ext| ext.insert(val));
    }

    pub fn get_extension<T: Clone + Send + Sync + 'static>(&self) -> Option<T> {
        self.with_extensions(|ext| ext.get::<T>().cloned())
            .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalleeDisplayName(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalleeOfflineMarker;

/// This inbound INVITE is a CLUSTER CHILD leg of an existing logical call:
/// the peer declared the root session id (CC UUI `leg=` intent) and the
/// proxy seam adopted it. The logical call's quality inspection, recording
/// attribution and CDR stay on the ROOT session at the dispatching node —
/// a second inspection tap here would duplicate the transcript with
/// side labels interpreted from local leg roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClusterChildLeg;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundTrunkContext {
    pub id: Option<i64>,
    pub name: String,
    pub dest: Option<String>,
    pub tenant_id: Option<i64>,
}

/// The routing rule (`RouteRule`) that matched and produced this call's
/// dialplan. Attached to dialplan hints by the route matcher so the CDR
/// reporter can attribute the call to the matched route. `id` is `None` for
/// rules that only exist in config files (no database row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRoute {
    pub id: Option<i64>,
    pub name: String,
}

/// A-leg (caller side) SIP peer address `ip:port`, captured from the inbound
/// INVITE's transport connection in `handle_invite`. Surfaced into the CDR as
/// `callerPeer` so consumers (Grafana plugin top-N, IP filtering) need no join
/// against the signaling table. Rides the cookie because early-failure CDRs
/// are reported before any SipSession exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerPeerContext(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrunkContext {
    pub id: Option<i64>,
    pub name: String,
    pub did_numbers: Vec<String>,
}

/// INVITE-with-Replaces takeover context (attended transfer executed by the
/// transferee UA per the REFER-Replaces chain). Carries the verbatim
/// `Replaces` header value from the inbound INVITE; the session attaches it
/// to the outbound target leg INVITE so the target UA auto-answers it as a
/// replacement of its own dialog (no second ring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferReplacesContext {
    pub header: String,
    /// True when this INVITE is being DELEGATED to the peer node that owns
    /// the replaced session (the takeover will run THERE): the outbound
    /// home-proxy hop must carry the Replaces header verbatim (the peer's
    /// gate consumes it and runs the local takeover) instead of the usual
    /// direct-to-registration routing.
    pub delegate_via_home_proxy: bool,
}
