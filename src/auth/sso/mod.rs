//! Generic SSO login broker (commerce feature).
//!
//! Brokers an enterprise SSO login into a native-app deep link using the
//! standard OAuth2 authorization-code shape (RFC 6749) with PKCE (RFC 7636):
//!
//! ```text
//! client → GET {base}/authorize?code_challenge&code_challenge_method=S256&state
//!        → 302 upstream login (+state=<sealed flow envelope>)
//! upstream → 302 {base}/callback?token=<jwt>&state=<echo>
//!        → verify HS256 JWT → seal single-use code envelope
//!        → 302 {redirect_url}?code=..&state=..   (e.g. myapp://callback)
//! client → POST {base}/token grant_type=authorization_code
//!        → { access_token, token_type, expires_in }
//! ```
//!
//! **Fully stateless**: flows and codes travel as HMAC-signed short-lived
//! envelopes (see [`code`]), so `/authorize`, `/callback` and `/token` can be
//! served by *any node* of a cluster with no shared storage — every node only
//! needs the same configured secrets.
//!
//! Endpoints mount only when `[sso].enabled = true` and the build has the
//! `commerce` feature. Wire contracts live in docs/sso_upstream_integration.md
//! and docs/sso_client_integration.md.

pub mod code;
#[cfg(test)]
mod e2e;
pub mod token;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde_json::Value;
use tracing::{info, warn};

use crate::auth::jwt_validator::{JwtError, JwtValidator};
use crate::config::{Config, SsoConfig, SsoJwtConfig};

use self::code::{ENVELOPE_ISSUER, UnsealError, now_epoch, seal, unseal};
use self::token::{
    IssuanceInput, RefreshStore, TOKEN_MODE_MINTED, TOKEN_MODE_PASSTHROUGH, mint_access_token,
    passthrough_response, token_error, token_success,
};

// ---------------------------------------------------------------------------
// Runtime state (immutable config snapshot + per-process refresh store)
// ---------------------------------------------------------------------------

pub struct SsoRuntime {
    pub provider: String,
    /// Full deep-link URL handed to the native app (`myapp://callback`, ...).
    pub redirect_url: String,
    pub base_path: String,
    pub upstream_login_url: String,
    /// Validates the upstream enterprise JWT at callback time.
    pub validator: JwtValidator,
    /// Validates rustpbx-MINTED access tokens on the API chain
    /// (`iss=rustpbx`, mint secret) — `None` in passthrough mode.
    pub mint_validator: Option<JwtValidator>,
    /// Secret sealing flow/code envelopes on every cluster node.
    pub envelope_secret: String,
    /// Secret used to sign tokens in minted mode; shared with
    /// `[proxy.jwt_auth].secret` so SIP/WS chains accept minted tokens.
    pub mint_secret: Option<String>,
    /// Enterprise claim name carrying the user id (also embedded in minted
    /// tokens so `[proxy.jwt_auth].user_id_claim` extraction stays aligned).
    pub user_id_claim: String,
    pub token_mode: String,
    pub code_ttl: Duration,
    pub flow_ttl: Duration,
    pub token_ttl_secs: u64,
    pub refresh_ttl_secs: u64,
    pub auto_provision: bool,
}

/// Per-process state. Only the refresh store is process-local; the entire
/// JWT-mode happy path is stateless (see module docs).
#[derive(Clone)]
pub struct SsoState {
    pub runtime: Arc<SsoRuntime>,
    refresh: RefreshStore,
}

impl SsoState {
    /// Build runtime state from config. Returns Err with a startup message
    /// when the section is enabled but incomplete — callers fail fast.
    pub fn from_config(config: &Config) -> Result<Self, String> {
        let Some(sso_cfg) = config.sso.clone() else {
            return Err("[sso] section missing".to_string());
        };
        // Prefer [proxy.jwt_auth].secret as the signing key for minted
        // tokens so SIP/WS validation accepts them unchanged.
        let jwt_auth_secret = config
            .proxy
            .jwt_auth
            .as_ref()
            .filter(|j| j.enabled && !j.secret.is_empty())
            .map(|j| j.secret.clone());
        Self::from_sso_config(&sso_cfg, jwt_auth_secret)
    }

    pub fn from_sso_config(
        sso_cfg: &SsoConfig,
        jwt_auth_secret: Option<String>,
    ) -> Result<Self, String> {
        if !sso_cfg.enabled {
            return Err("[sso] not enabled".to_string());
        }
        match sso_cfg.provider.as_str() {
            "jwt" => {}
            other => return Err(format!("[sso] unknown provider {other:?}")),
        }
        let jwt = sso_cfg.jwt.as_ref().ok_or(
            "[sso] provider \"jwt\" requires a [sso.jwt] section (secret, upstream_login_url)",
        )?;
        validate_jwt_config(jwt)?;
        let redirect_url = sso_cfg
            .redirect_url
            .clone()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string())
            .ok_or("[sso] enabled but redirect_url is empty")?;
        if !redirect_url.contains("://") {
            return Err(format!(
                "[sso] redirect_url {redirect_url:?} must be a full URL (e.g. myapp://callback)"
            ));
        }

        let token_mode = jwt.token_mode.as_str();
        // Envelope sealing must work cluster-wide: every node needs the same
        // secret ([proxy.jwt_auth].secret preferred, else [sso.jwt].secret).
        let envelope_secret = jwt_auth_secret
            .clone()
            .unwrap_or_else(|| jwt.secret.clone());
        let mint_secret = if token_mode == TOKEN_MODE_MINTED {
            Some(envelope_secret.clone())
        } else {
            None
        };
        let base_path = normalize_base_path(&sso_cfg.base_path)
            .ok_or("[sso] base_path \"/\" is not allowed (use e.g. \"/sso\")")?;
        // Validates rustpbx-minted access tokens on local chains (iss=rustpbx,
        // signed with the mint secret) — distinct from the upstream validator.
        let mint_validator = if token_mode == TOKEN_MODE_MINTED {
            let mut jwt_minted = jwt.clone();
            jwt_minted.secret = mint_secret
                .clone()
                .unwrap_or_else(|| jwt_minted.secret.clone());
            jwt_minted.issuer = Some("rustpbx".to_string());
            jwt_minted.audience = None;
            Some(build_validator(&jwt_minted))
        } else {
            None
        };

        Ok(Self {
            runtime: Arc::new(SsoRuntime {
                provider: sso_cfg.provider.clone(),
                redirect_url,
                base_path,
                upstream_login_url: jwt.upstream_login_url.trim().to_string(),
                validator: build_validator(jwt),
                envelope_secret,
                mint_secret,
                mint_validator,
                user_id_claim: jwt.user_id_claim.clone(),
                token_mode: token_mode.to_string(),
                code_ttl: Duration::from_secs(sso_cfg.code_ttl_secs.max(1)),
                flow_ttl: Duration::from_secs(sso_cfg.flow_ttl_secs.max(1)),
                token_ttl_secs: jwt.token_ttl_secs,
                refresh_ttl_secs: jwt.refresh_token_ttl_secs,
                auto_provision: sso_cfg.auto_provision,
            }),
            refresh: RefreshStore::new(),
        })
    }

    /// Validate an upstream bearer token through the shared chain. Returns
    /// `(user_id, claims)`; used at `/callback` (incoming = upstream JWT).
    pub fn authenticate_token(&self, token: &str) -> Result<(String, Value), JwtError> {
        let claims = self.runtime.validator.validate(token)?;
        match self.runtime.validator.extract_user_id(&claims) {
            Some(user_id) => Ok((user_id, claims)),
            None => Err(JwtError::MissingUserId),
        }
    }

    /// Validate a Bearer presented on LOCAL chains (`/api`): accepts both
    /// rustpbx-minted tokens (`iss=rustpbx`, mint secret) and enterprise
    /// passthrough tokens. Minted first — its signature/issuer space is
    /// disjoint from the upstream on purpose.
    pub fn authenticate_api_token(&self, token: &str) -> Result<(String, Value), JwtError> {
        if let Some(mint_validator) = &self.runtime.mint_validator
            && let Ok(claims) = mint_validator.validate(token)
            && let Some(user_id) = mint_validator.extract_user_id(&claims)
        {
            return Ok((user_id, claims));
        }
        self.authenticate_token(token)
    }

    fn deep_link(&self, query: &str) -> String {
        let base = self.runtime.redirect_url.as_str();
        let sep = if base.contains('?') { '&' } else { '?' };
        format!("{base}{sep}{query}")
    }

    fn issue_response_for(&self, input: IssuanceInput<'_>) -> Response {
        match self.runtime.token_mode.as_str() {
            TOKEN_MODE_MINTED => {
                let Some(secret) = self.runtime.mint_secret.clone() else {
                    return token_error("server_error");
                };
                let sid = uuid::Uuid::new_v4().simple().to_string();
                let Ok((access_token, exp)) = mint_access_token(
                    &secret,
                    input.user_id,
                    &self.runtime.user_id_claim,
                    input.upstream_claims,
                    &sid,
                    self.runtime.token_ttl_secs,
                ) else {
                    return token_error("server_error");
                };
                let mut body = serde_json::json!({
                    "access_token": access_token,
                    "token_type": "Bearer",
                    "expires_in": exp - now_epoch(),
                });
                if self.runtime.refresh_ttl_secs > 0
                    && let Some(rt) = self.refresh.insert(
                        sid,
                        input.user_id.to_string(),
                        input.upstream_claims.clone(),
                        Duration::from_secs(self.runtime.refresh_ttl_secs),
                    )
                {
                    body["refresh_token"] = Value::String(rt);
                }
                token_success(body)
            }
            _ => token_success(passthrough_response(input)),
        }
    }
}

fn validate_jwt_config(jwt: &SsoJwtConfig) -> Result<(), String> {
    if jwt.secret.trim().is_empty() {
        return Err("[sso.jwt] secret is empty".into());
    }
    if jwt.upstream_login_url.trim().is_empty() {
        return Err("[sso.jwt] upstream_login_url is empty".into());
    }
    match jwt.token_mode.as_str() {
        TOKEN_MODE_PASSTHROUGH | TOKEN_MODE_MINTED => Ok(()),
        other => Err(format!(
            "[sso.jwt] unknown token_mode {other:?} (passthrough|minted)"
        )),
    }
}

fn build_validator(jwt: &SsoJwtConfig) -> JwtValidator {
    JwtValidator::new(&crate::config::JwtAuthConfig {
        enabled: true,
        secret: jwt.secret.clone(),
        user_id_claim: jwt.user_id_claim.clone(),
        issuer: jwt.issuer.clone(),
        audience: jwt.audience.clone(),
        sip_header_name: "X-Auth-Token".to_string(),
        check_local_user: false,
        ws_token_param: "token".to_string(),
        dev_mint_enabled: false,
    })
}

/// Normalize `[sso].base_path`: ensure a single leading slash, no trailing
/// slash (axum `nest` rejects it). `None` = unusable value (`"/"`).
fn normalize_base_path(path: &str) -> Option<String> {
    let trimmed = path.trim();
    let mut out = if trimmed.is_empty() {
        "/sso".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    if out == "/" { None } else { Some(out) }
}

// ---------------------------------------------------------------------------
// Router mounting (double gate: commerce cfg + enabled flag)
// ---------------------------------------------------------------------------

/// Mount the SSO endpoints when configured. Returns None otherwise — callers
/// merge nothing and the routes simply do not exist (404).
///
/// Routes nest under `[sso].base_path` (default `/sso`): `/sso/authorize`,
/// `/sso/callback`, `/sso/token`.
pub fn mount_router(config: &Config) -> Option<Router> {
    let state = match SsoState::from_config(config) {
        Ok(state) => state,
        Err(reason) => {
            if config.sso.as_ref().is_some_and(|s| s.enabled) {
                warn!("SSO disabled: {reason}");
            }
            return None;
        }
    };
    warn_config_drift(config, &state);
    info!(
        "SSO broker mounted at {} (provider={}, token_mode={}, stateless envelopes)",
        state.runtime.base_path, state.runtime.provider, state.runtime.token_mode
    );
    let base = state.runtime.base_path.clone();
    Some(Router::new().nest(&base, inner_router(state)))
}

fn inner_router(state: SsoState) -> Router {
    Router::new()
        .route("/authorize", get(authorize_handler))
        .route("/callback", get(callback_handler))
        .route("/token", post(token_handler))
        .with_state(state)
}

/// Startup sanity checks for the SIP/WS interplay. Misalignment is not fatal
/// (API-only deployments may not care) but silently breaks token acceptance
/// on the SIP chains, so it must at least be loud.
fn warn_config_drift(config: &Config, state: &SsoState) {
    let Some(jwt) = config.sso.as_ref().and_then(|s| s.jwt.as_ref()) else {
        return;
    };
    match config.proxy.jwt_auth.as_ref().filter(|j| j.enabled) {
        None => warn!(
            "SSO broker: [proxy.jwt_auth] missing/disabled — SIP (X-Auth-Token) and WS (?token=) \
             chains will REJECT SSO tokens until it is enabled with the matching secret"
        ),
        Some(ja) => {
            if state.runtime.token_mode == TOKEN_MODE_PASSTHROUGH && ja.secret != jwt.secret {
                warn!(
                    "SSO broker: [proxy.jwt_auth].secret differs from [sso.jwt].secret — \
                     passthrough tokens that pass /api auth will fail SIP/WS authentication"
                );
            }
            if ja.user_id_claim != jwt.user_id_claim {
                warn!(
                    "SSO broker: user_id_claim mismatch ([proxy.jwt_auth]={} vs [sso.jwt]={}) — \
                     SIP identity extraction will fail",
                    ja.user_id_claim, jwt.user_id_claim
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// RFC 6749 §4.1.1 authorization request from the native app.
async fn authorize_handler(
    State(state): State<SsoState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(challenge) = non_empty(&params, "code_challenge") else {
        return bad_request("missing code_challenge");
    };
    match params.get("code_challenge_method").map(|s| s.as_str()) {
        None | Some("S256") => {}
        Some(other) => return bad_request(&format!("unsupported code_challenge_method {other:?}")),
    }
    let Some(client_state) = non_empty(&params, "state") else {
        return bad_request("missing state");
    };

    // Sealed flow envelope replaces server-side flow storage: whichever node
    // receives the upstream callback can verify it independently.
    let flow_claims = serde_json::json!({
        "iss": ENVELOPE_ISSUER,
        "k": "flow",
        "cst": client_state,
        "chl": challenge,
        "exp": now_epoch() + state.runtime.flow_ttl.as_secs() as i64,
    });
    let flow_envelope = seal(&flow_claims, &state.runtime.envelope_secret);

    // Hand control to the enterprise login page; it must echo `state` back.
    let login_url = state.runtime.upstream_login_url.clone();
    let sep = if login_url.contains('?') { '&' } else { '?' };
    let location = format!(
        "{login_url}{sep}state={}",
        urlencoding::encode(&flow_envelope)
    );
    Redirect::to(&location).into_response()
}

fn non_empty(params: &std::collections::HashMap<String, String>, key: &str) -> Option<String> {
    params
        .get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CACHE_CONTROL, "no-store")],
        format!("{message}\n"),
    )
        .into_response()
}

/// Upstream redirect landing: validates the enterprise JWT, binds a single-use
/// sealed code to the pending flow, and deep-links back to the native app.
async fn callback_handler(
    State(state): State<SsoState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    // Upstream-reported failure path: surface to the app via the deep link.
    if let Some(err) = non_empty(&params, "error") {
        if let Some(flow_envelope) = non_empty(&params, "state")
            && let Ok(flow) = unseal(&flow_envelope, &state.runtime.envelope_secret, "flow")
        {
            let q = format!(
                "error={}&state={}",
                urlencoding::encode(&err),
                urlencoding::encode(flow.get("cst").and_then(|v| v.as_str()).unwrap_or(""))
            );
            return Redirect::to(&state.deep_link(&q)).into_response();
        }
        return bad_request("authorization failed");
    }

    let (Some(token), Some(flow_envelope)) =
        (non_empty(&params, "token"), non_empty(&params, "state"))
    else {
        return bad_request("missing token/state");
    };

    let flow = match unseal(&flow_envelope, &state.runtime.envelope_secret, "flow") {
        Ok(flow) => flow,
        Err(e @ (UnsealError::Expired | UnsealError::BadSignature | UnsealError::Malformed)) => {
            tracing::debug!(error = %e, "sso flow envelope rejected");
            return bad_request("invalid or expired state");
        }
        Err(UnsealError::WrongKind) => return bad_request("invalid or expired state"),
    };

    let (user_id, claims) = match state.authenticate_token(&token) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "sso callback rejected token");
            return bad_request("invalid credentials");
        }
    };

    // Sealed code envelope: integrity-protected upstream token + PKCE
    // binding; short-lived (`code_ttl`) so replay risk matches OAuth norms.
    let code_claims = serde_json::json!({
        "iss": ENVELOPE_ISSUER,
        "k": "code",
        "cst": flow.get("cst").cloned().unwrap_or(Value::Null),
        "chl": flow.get("chl").cloned().unwrap_or(Value::Null),
        "at": token,
        "uid": user_id,
        "ecl": claims,
        "exp": now_epoch() + state.runtime.code_ttl.as_secs() as i64,
    });
    let code = seal(&code_claims, &state.runtime.envelope_secret);

    let q = format!(
        "code={}&state={}",
        urlencoding::encode(&code),
        urlencoding::encode(flow.get("cst").and_then(|v| v.as_str()).unwrap_or(""))
    );
    (
        StatusCode::FOUND,
        [(header::X_FRAME_OPTIONS, "DENY")],
        Redirect::to(&state.deep_link(&q)),
    )
        .into_response()
}

/// RFC 6749 §3.2 token endpoint (form-encoded body).
async fn token_handler(State(state): State<SsoState>, body: String) -> Response {
    let form: std::collections::HashMap<String, String> = match form_urlencoded_parse(&body) {
        Ok(form) => form,
        Err(_) => return token_error("invalid_request"),
    };

    match form.get("grant_type").map(|s| s.as_str()) {
        Some("authorization_code") => {
            let (Some(code), Some(verifier)) =
                (non_empty(&form, "code"), non_empty(&form, "code_verifier"))
            else {
                return token_error("invalid_request");
            };
            let code_claims = match unseal(&code, &state.runtime.envelope_secret, "code") {
                Ok(c) => c,
                Err(_) => return token_error("invalid_grant"),
            };
            let (Some(challenge), Some(access_token)) = (
                code_claims.get("chl").and_then(|v| v.as_str()),
                code_claims.get("at").and_then(|v| v.as_str()),
            ) else {
                return token_error("invalid_grant");
            };
            if !code::verify_pkce_s256(challenge, &verifier) {
                return token_error("invalid_grant");
            }
            let uid = code_claims
                .get("uid")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ecl = code_claims.get("ecl").cloned().unwrap_or(Value::Null);
            state.issue_response_for(IssuanceInput {
                access_token,
                user_id: uid,
                upstream_claims: &ecl,
                fallback_ttl_secs: state.runtime.token_ttl_secs,
            })
        }
        Some("refresh_token") => {
            if state.runtime.token_mode != TOKEN_MODE_MINTED || state.runtime.refresh_ttl_secs == 0
            {
                return token_error("unsupported_grant_type");
            }
            let Some(rt) = non_empty(&form, "refresh_token") else {
                return token_error("invalid_request");
            };
            match state.refresh.rotate(&rt) {
                Some((new_rt, sid, subject, claims)) => {
                    let Some(secret) = state.runtime.mint_secret.clone() else {
                        return token_error("server_error");
                    };
                    let Ok((access_token, exp)) = mint_access_token(
                        &secret,
                        &subject,
                        &state.runtime.user_id_claim,
                        &claims,
                        &sid,
                        state.runtime.token_ttl_secs,
                    ) else {
                        return token_error("server_error");
                    };
                    token_success(serde_json::json!({
                        "access_token": access_token,
                        "token_type": "Bearer",
                        "expires_in": exp - now_epoch(),
                        "refresh_token": new_rt,
                    }))
                }
                None => token_error("invalid_grant"),
            }
        }
        _ => token_error("unsupported_grant_type"),
    }
}

/// Minimal application/x-www-form-urlencoded parser (lenient on encoding).
fn form_urlencoded_parse(body: &str) -> Result<std::collections::HashMap<String, String>, ()> {
    let mut out = std::collections::HashMap::new();
    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let k = percent_decode(k).ok_or(())?;
        let v = percent_decode(v).ok_or(())?;
        out.insert(k, v);
    }
    Ok(out)
}

fn percent_decode(input: &str) -> Option<String> {
    urlencoding::decode(input).ok().map(|c| c.into_owned())
}

// ---------------------------------------------------------------------------
// Console/API integration: accept SSO bearer tokens on /api routes
// ---------------------------------------------------------------------------

/// Validate a Bearer token issued by this broker (or handed through by the
/// enterprise IdP) and map it onto a local console user.
///
/// Mapping order: local user whose `username` equals the enterprise user id,
/// else one whose `email` equals the token's `email` claim; with
/// `[sso].auto_provision = true` unknown identities get a JIT-created,
/// role-less account (`auth_source = "sso"`); otherwise None.
///
/// Stateless end-to-end: any node validates with local secrets alone.
pub async fn resolve_user_for_bearer(
    console: &crate::console::ConsoleState,
    bearer: &str,
) -> Option<crate::models::user::Model> {
    let app_state = console.app_state()?;
    let state = SsoState::from_config(app_state.config()).ok()?;
    let (user_id, claims) = state.authenticate_api_token(bearer).ok()?;

    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let db = console.db();
    let mut found = crate::models::user::Entity::find()
        .filter(crate::models::user::Column::Username.eq(&user_id))
        .one(db)
        .await
        .ok()
        .flatten();
    if found.is_none()
        && let Some(email) = claims.get("email").and_then(|v| v.as_str())
    {
        found = crate::models::user::Entity::find()
            .filter(crate::models::user::Column::Email.eq(email.to_lowercase()))
            .one(db)
            .await
            .ok()
            .flatten();
    }
    if let Some(user) = found {
        return user.is_active.then_some(user);
    }
    if !state.runtime.auto_provision {
        tracing::debug!("sso identity {user_id} has no local user (auto_provision=false)");
        return None;
    }

    provision_sso_user(console, &user_id, &claims).await
}

async fn provision_sso_user(
    console: &crate::console::ConsoleState,
    user_id: &str,
    claims: &Value,
) -> Option<crate::models::user::Model> {
    use sea_orm::{ActiveModelTrait, Set};

    let email_from_claims = claims
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let email = if email_from_claims.is_empty() {
        format!("{user_id}@sso.invalid")
    } else {
        email_from_claims
    };

    let now = chrono::Utc::now();
    let active = crate::models::user::ActiveModel {
        email: Set(email.clone()),
        username: Set(user_id.to_string()),
        // Unusable password: no password login, SSO only.
        password_hash: Set(String::new()),
        reset_token: Set(None),
        reset_token_expires: Set(None),
        last_login_at: Set(Some(now)),
        last_login_ip: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        is_active: Set(true),
        is_staff: Set(false),
        is_superuser: Set(false),
        mfa_enabled: Set(false),
        mfa_secret: Set(None),
        auth_source: Set("sso".into()),
        ..Default::default()
    };

    match active.insert(console.db()).await {
        Ok(model) => {
            info!("provisioned sso user {} ({email})", model.username);
            Some(model)
        }
        Err(e) => {
            tracing::warn!(%e, "failed to provision sso user {user_id}");
            None
        }
    }
}
