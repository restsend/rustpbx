//! End-to-end tests for the SSO broker, exercised through the **public
//! `mount_router`** so the tests cover exactly what production mounts:
//! `base_path` nesting, the enabled gate, and the stateless happy path
//! (jwt handoff / passthrough mode).
//!
//! Every step re-derives all security state from sealed envelopes, which is
//! what lets `/authorize`, `/callback` and `/token` run on different cluster
//! nodes with no shared storage.

use axum::body::Body;
use base64::Engine;
use http::{Request, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::config::{Config, SsoConfig, SsoJwtConfig};

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const BASE: &str = "/sso";

fn challenge_for(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier))
}

fn make_sso_config() -> SsoConfig {
    SsoConfig {
        enabled: true,
        base_path: BASE.into(),
        provider: "jwt".into(),
        redirect_url: Some("myapp://auth/finish".into()),
        auto_provision: false,
        code_ttl_secs: 60,
        flow_ttl_secs: 600,
        jwt: Some(SsoJwtConfig {
            secret: "test-secret".into(),
            token_mode: "passthrough".into(),
            user_id_claim: "userId".into(),
            issuer: Some("https://sso.example.com".into()),
            audience: None,
            upstream_login_url: "https://sso.example.com/login?client=rustpbx".into(),
            token_ttl_secs: 3600,
            refresh_token_ttl_secs: 0,
        }),
    }
}

fn config_with_sso(sso: Option<SsoConfig>) -> Config {
    let mut cfg = Config::default();
    cfg.sso = sso;
    cfg
}

fn mounted_app() -> axum::Router {
    super::mount_router(&config_with_sso(Some(make_sso_config())))
        .expect("enabled+valid [sso] must mount")
}

fn query_param(uri: &str, key: &str) -> String {
    uri.split('?')
        .nth(1)
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
        .map(|v| {
            urlencoding::decode(v)
                .map(|c| c.into_owned())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

async fn run_get(app: axum::Router, path_and_query: &str) -> (StatusCode, String, Vec<u8>) {
    let req = Request::builder()
        .uri(path_and_query)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let location = resp
        .headers()
        .get(http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap_or_default()
        .to_vec();
    (status, location, body)
}

fn mint_upstream_jwt(exp_in_secs: i64) -> String {
    crate::auth::jwt_validator::generate_hs256_jwt(
        &serde_json::json!({
            "iss": "https://sso.example.com",
            "userId": "1001",
            "email": "alice@corp.com",
            "exp": super::token::now_epoch() + exp_in_secs,
        }),
        "test-secret",
    )
}

async fn drive_to_code(app: axum::Router, client_state: &str) -> (StatusCode, String, String) {
    // Node 1: authorize seals a flow envelope into the upstream redirect.
    let (status, location, _) = run_get(
        app.clone(),
        &format!(
            "{BASE}/authorize?code_challenge={}&code_challenge_method=S256&state={}",
            urlencoding::encode(&challenge_for(VERIFIER)),
            urlencoding::encode(client_state),
        ),
    )
    .await;
    if !status.is_redirection() {
        return (status, String::new(), String::new());
    }
    assert!(
        location.starts_with("https://sso.example.com/login"),
        "{location}"
    );
    let flow_envelope = query_param(&location, "state");
    assert!(!flow_envelope.is_empty());

    // Node 2 (different instance, same secrets): callback verifies envelope +
    // enterprise JWT and seals the code.
    let cb = format!(
        "{BASE}/callback?token={}&state={}",
        urlencoding::encode(&mint_upstream_jwt(600)),
        urlencoding::encode(&flow_envelope),
    );
    let (status, location, _) = run_get(app.clone(), &cb).await;
    assert_eq!(status, StatusCode::FOUND);
    assert!(
        location.starts_with("myapp://auth/finish"),
        "deep link must honor configured redirect_url: {location}"
    );
    assert_eq!(query_param(&location, "state"), client_state);
    let code_envelope = query_param(&location, "code");

    (status, location, code_envelope)
}

/// Full happy path across three handler invocations + endpoint layout.
#[tokio::test]
async fn passthrough_flow_across_handlers() {
    let app = mounted_app();

    // Mounting contract: nested under base_path ONLY — root paths 404.
    let (status, _, _) = run_get(
        app.clone(),
        "/authorize?code_challenge=x&code_challenge_method=S256&state=z",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "/authorize must not exist at root"
    );

    let (status, _, _) = run_get(app.clone(), "/sso/authorize").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "missing params → 400, not 404"
    );

    let (_, _, code_envelope) = drive_to_code(app.clone(), "client-st-1").await;

    // Node 3: token exchange returns the enterprise JWT verbatim.
    let body = format!(
        "grant_type=authorization_code&code={}&code_verifier={}",
        urlencoding::encode(&code_envelope),
        VERIFIER,
    );
    let req = Request::builder()
        .method(http::Method::POST)
        .uri(&format!("{BASE}/token"))
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json["access_token"].as_str().is_some());
    assert_eq!(json["token_type"], "Bearer");
    assert!(json.get("refresh_token").is_none());
    // expires_in derives from the enterprise JWT exp (+600), never 0 here.
    let expires_in = json["expires_in"].as_i64().unwrap();
    assert!((550..=600).contains(&expires_in), "expires_in={expires_in}");
}

#[tokio::test]
async fn disabled_sso_mounts_nothing() {
    assert!(super::mount_router(&config_with_sso(None)).is_none());

    let mut disabled = make_sso_config();
    disabled.enabled = false;
    assert!(super::mount_router(&config_with_sso(Some(disabled))).is_none());
}

#[tokio::test]
async fn invalid_base_path_is_rejected() {
    let mut cfg = make_sso_config();
    cfg.base_path = "/".into();
    assert!(super::SsoState::from_sso_config(&cfg, None).is_err());

    cfg.base_path = "sso/".into(); // tolerated → normalized to /sso
    let state = super::SsoState::from_sso_config(&cfg, None).expect("trailing slash ok");
    assert_eq!(state.runtime.base_path, "/sso");
}

#[tokio::test]
async fn callback_rejects_forged_or_expired_flow() {
    let app = mounted_app();
    let upstream_jwt = mint_upstream_jwt(60);

    // forged envelope (bad signature) must not yield a code
    let forged = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(br#"{"k":"flow","cst":"x","chl":"y"}"#);
    let cb = format!(
        "{BASE}/callback?token={}&state={}.forgedsig",
        urlencoding::encode(&upstream_jwt),
        urlencoding::encode(&forged),
    );
    let (status, _, _) = run_get(app, &cb).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn token_rejects_pkce_mismatch() {
    let app = mounted_app();

    let (_, location, _) = run_get(
        app.clone(),
        &format!(
            "{BASE}/authorize?code_challenge={}&code_challenge_method=S256&state=st2",
            urlencoding::encode(&challenge_for(VERIFIER)),
        ),
    )
    .await;
    let flow_envelope = query_param(&location, "state");

    let cb = format!(
        "{BASE}/callback?token={}&state={}",
        urlencoding::encode(&mint_upstream_jwt(600)),
        urlencoding::encode(&flow_envelope),
    );
    let (_, location, _) = run_get(app.clone(), &cb).await;
    let code_envelope = query_param(&location, "code");

    // Wrong verifier → invalid_grant even though code/envelope are valid.
    let wrong = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-wrongverifier-padding-xx";
    let body = format!(
        "grant_type=authorization_code&code={}&code_verifier={}",
        urlencoding::encode(&code_envelope),
        urlencoding::encode(wrong),
    );
    let req = Request::builder()
        .method(http::Method::POST)
        .uri(&format!("{BASE}/token"))
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// Minted mode: access tokens (`iss=rustpbx`, mint secret) must validate on
/// the local API chain even when signed with `[proxy.jwt_auth].secret`,
/// different from `[sso.jwt].secret`.
#[tokio::test]
async fn minted_token_validates_on_api_chain() {
    let mut cfg = make_sso_config();
    cfg.jwt.as_mut().unwrap().token_mode = "minted".into();
    let state = super::SsoState::from_sso_config(&cfg, Some("jwt-auth-secret".into()))
        .expect("valid minted config");

    let claims = serde_json::json!({"userId": "1001", "email": "a@b.c"});
    let (token, _) = super::token::mint_access_token(
        state.runtime.mint_secret.as_deref().unwrap(),
        "1001",
        &state.runtime.user_id_claim,
        &claims,
        "sid-1",
        300,
    )
    .unwrap();

    // The UPSTREAM validator rejects it...
    assert!(state.authenticate_token(&token).is_err());
    // ...but the API chain accepts both minted tokens ...
    let (uid, decoded) = state.authenticate_api_token(&token).unwrap();
    assert_eq!(uid, "1001");
    assert_eq!(decoded["iss"], "rustpbx");
    // ...and enterprise passthrough tokens.
    let (uid2, _) = state
        .authenticate_api_token(&mint_upstream_jwt(300))
        .unwrap();
    assert_eq!(uid2, "1001");
}
