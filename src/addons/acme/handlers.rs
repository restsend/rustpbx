use super::{AcmeState, AcmeStatus};
use crate::app::AppState;
use axum::{
    Extension,
    extract::{Json, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
};
use serde::{Deserialize, Serialize};
use std::path::Path as StdPath;
use toml_edit::{DocumentMut, value};
use tracing::{error, info, warn};

#[derive(Deserialize)]
pub struct RequestCertPayload {
    domain: String,
    email: String,
    enable_https: bool,
    enable_sip_tls: bool,
}

#[derive(Serialize)]
pub struct CertInfo {
    domain: String,
    path: String,
    created_at: String,
}

pub async fn status(Extension(acme_state): Extension<AcmeState>) -> impl IntoResponse {
    let status = acme_state.status.read().unwrap().clone();
    Json(status)
}

pub async fn ui_index(
    State(state): State<AppState>,
    Extension(acme_state): Extension<AcmeState>,
) -> impl IntoResponse {
    #[cfg(feature = "console")]
    {
        if let Some(console) = &state.console {
            let certs = list_certificates().unwrap_or_default();
            let status = acme_state.status.read().unwrap().clone();
            return console.render(
                "acme/acme_index.html",
                serde_json::json!({
                    "certs": certs,
                    "status": status,
                    "nav_active": "SSL Certificates"
                }),
            );
        }
    }

    Html("Console feature not enabled".to_string()).into_response()
}

fn list_certificates() -> anyhow::Result<Vec<CertInfo>> {
    let cert_dir = StdPath::new("config/certs");
    if !cert_dir.exists() {
        return Ok(vec![]);
    }

    let mut certs = Vec::new();
    for entry in std::fs::read_dir(cert_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("crt") {
            let domain = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let metadata = entry.metadata()?;
            let created = metadata.created().unwrap_or(std::time::SystemTime::now());
            let created_at = humantime::format_rfc3339(created).to_string();

            certs.push(CertInfo {
                domain,
                path: path.to_string_lossy().to_string(),
                created_at,
            });
        }
    }
    // Sort by creation time descending
    certs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(certs)
}

pub async fn challenge(
    Path(token): Path<String>,
    Extension(acme_state): Extension<AcmeState>,
) -> impl IntoResponse {
    info!("Handling ACME challenge request for token: {}", token);
    let challenges = acme_state.challenges.read().unwrap();
    if let Some(response) = challenges.get(&token) {
        info!("Found challenge response for token: {}", token);
        (StatusCode::OK, response.clone())
    } else {
        warn!("Challenge token not found: {}", token);
        (StatusCode::NOT_FOUND, "Not Found".to_string())
    }
}

pub async fn request_cert(
    State(state): State<AppState>,
    Extension(acme_state): Extension<AcmeState>,
    Json(payload): Json<RequestCertPayload>,
) -> impl IntoResponse {
    let domain = payload.domain.clone();
    let email = payload.email.clone();
    let enable_https = payload.enable_https;
    let enable_sip_tls = payload.enable_sip_tls;

    info!(
        "Received certificate request for domain: {}, email: {}",
        domain, email
    );

    {
        let mut status = acme_state.status.write().unwrap();
        *status = AcmeStatus::Running("Starting...".to_string());
    }

    let acme_state_clone = acme_state.clone();

    crate::utils::spawn(async move {
        if let Err(e) = process_acme(
            domain,
            email,
            enable_https,
            enable_sip_tls,
            acme_state_clone.clone(),
            state,
        )
        .await
        {
            error!("ACME processing failed: {}", e);
            let mut status = acme_state_clone.status.write().unwrap();
            *status = AcmeStatus::Error(e.to_string());
        }
    });

    serde_json::json!({ "status": "started", "message": "Certificate request started in background." }).to_string()
}

async fn process_acme(
    domain: String,
    email: String,
    enable_https: bool,
    enable_sip_tls: bool,
    acme_state: AcmeState,
    app_state: AppState,
) -> anyhow::Result<()> {
    use instant_acme::{
        Account, AuthorizationStatus, Identifier, NewAccount, NewOrder, OrderStatus,
    };

    info!("Starting ACME process for domain: {}", domain);
    {
        let mut status = acme_state.status.write().unwrap();
        *status = AcmeStatus::Running(format!("Creating account for {}", email));
    }

    let url = "https://acme-v02.api.letsencrypt.org/directory";

    let (account, _) = Account::builder()?
        .create(
            &NewAccount {
                contact: &[&format!("mailto:{}", email)],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            url.to_string(),
            None,
        )
        .await?;

    info!("ACME account created/retrieved");
    {
        let mut status = acme_state.status.write().unwrap();
        *status = AcmeStatus::Running(format!("Creating order for {}", domain));
    }

    let mut order = account
        .new_order(&NewOrder::new(&[Identifier::Dns(domain.clone())]))
        .await?;

    info!("ACME order created");
    {
        let mut status = acme_state.status.write().unwrap();
        *status = AcmeStatus::Running("Solving challenges...".to_string());
    }

    loop {
        let mut pending_auth_url = None;
        let mut token = String::new();

        {
            let mut authorizations = order.authorizations();
            while let Some(auth_result) = authorizations.next().await {
                let mut auth = auth_result?;
                if auth.status == AuthorizationStatus::Pending {
                    pending_auth_url = Some(auth.url().to_string());
                    info!("Solving challenge for auth url: {}", auth.url());
                    token = solve_http01_challenge(&mut auth, &acme_state).await?;
                    break;
                }
            }
        }

        if let Some(url) = pending_auth_url {
            let mut retries = 0;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

                if let Err(e) = order.refresh().await {
                    warn!("Failed to refresh order: {}", e);
                }

                let order_status = order.state().status;
                info!("Order status: {:?}", order_status);

                // Re-fetch authorizations from the refreshed order
                let mut authorizations = order.authorizations();
                let mut status = AuthorizationStatus::Pending;
                let mut found = false;
                let mut challenge_status = None;
                let mut challenge_error = None;

                while let Some(auth_res) = authorizations.next().await {
                    let auth = auth_res?;
                    info!(
                        "Comparing auth url: {} with target: {} auth: {:?}",
                        auth.url(),
                        url,
                        auth.status
                    );
                    if auth.url() == url {
                        status = auth.status;
                        found = true;

                        // Manual check if status is Pending, as instant-acme might be caching or stale
                        if status == AuthorizationStatus::Pending {
                            info!("Manual check for auth url: {}", url);
                            match reqwest::get(url.clone()).await {
                                Ok(resp) => {
                                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                                        if let Some(s) = json.get("status").and_then(|s| s.as_str())
                                        {
                                            info!("Manual check status: {}", s);
                                            if s == "invalid" {
                                                status = AuthorizationStatus::Invalid;
                                                // Try to extract error from challenges
                                                if let Some(challenges) = json
                                                    .get("challenges")
                                                    .and_then(|c| c.as_array())
                                                {
                                                    for c in challenges {
                                                        if let Some(c_status) =
                                                            c.get("status").and_then(|s| s.as_str())
                                                        {
                                                            if c_status == "invalid" {
                                                                if let Some(err) = c.get("error") {
                                                                    challenge_error =
                                                                        Some(format!("{}", err));
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => warn!("Manual check failed: {}", e),
                            }
                        }

                        if let Some(challenge) = auth
                            .challenges
                            .iter()
                            .find(|c| c.r#type == instant_acme::ChallengeType::Http01)
                        {
                            challenge_status = Some(challenge.status);
                            if let Some(error) = &challenge.error {
                                challenge_error = Some(format!("{}", error));
                            }
                        }
                        break;
                    }
                }

                if !found {
                    return Err(anyhow::anyhow!("Authorization not found in order"));
                }

                info!("Checking authorization status: {:?}", status);
                {
                    let mut s = acme_state.status.write().unwrap();
                    *s = AcmeStatus::Running(format!("Authorization status: {:?}", status));
                }

                if let Some(status) = challenge_status {
                    info!("HTTP-01 Challenge status: {:?}", status);
                }
                if let Some(error) = &challenge_error {
                    error!("Challenge error: {}", error);
                    let mut s = acme_state.status.write().unwrap();
                    *s = AcmeStatus::Running(format!("Challenge error: {}", error));
                }

                if status == AuthorizationStatus::Valid {
                    break;
                }
                if status == AuthorizationStatus::Invalid {
                    let msg =
                        challenge_error.unwrap_or_else(|| "Authorization invalid".to_string());
                    return Err(anyhow::anyhow!(msg));
                }

                if order_status == OrderStatus::Invalid {
                    let msg = challenge_error.unwrap_or_else(|| {
                        if let Some(e) = &order.state().error {
                            format!("Order invalid: {}", e)
                        } else {
                            "Order invalid".to_string()
                        }
                    });
                    return Err(anyhow::anyhow!(msg));
                }

                retries += 1;
                if retries > 60 {
                    return Err(anyhow::anyhow!("Authorization timed out"));
                }
            }

            {
                let mut challenges = acme_state.challenges.write().unwrap();
                challenges.remove(&token);
            }
        } else {
            break;
        }
    }

    let mut params = rcgen::CertificateParams::new(vec![domain.clone()])?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, domain.clone());

    let key_pair = rcgen::KeyPair::generate()?;
    let private_key_pem = key_pair.serialize_pem();
    let csr = params.serialize_request(&key_pair)?;

    let state = order.refresh().await?;
    if state.status == OrderStatus::Ready {
        info!("Finalizing order with CSR");
        {
            let mut status = acme_state.status.write().unwrap();
            *status = AcmeStatus::Running("Finalizing order...".to_string());
        }
        order.finalize_csr(&csr.der()).await?;
    }

    let mut retries = 0;
    loop {
        let state = order.state();
        {
            let mut status = acme_state.status.write().unwrap();
            *status = AcmeStatus::Running(format!("Order status: {:?}", state.status));
        }
        if state.status == OrderStatus::Valid {
            break;
        }
        if state.status == OrderStatus::Invalid {
            return Err(anyhow::anyhow!("Order invalid"));
        }

        if let Some(cert) = order.certificate().await? {
            save_cert_and_update_config(
                &cert,
                &private_key_pem,
                &domain,
                enable_https,
                enable_sip_tls,
                &app_state,
            )?;
            return Ok(());
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        order.refresh().await?;

        retries += 1;
        if retries > 30 {
            return Err(anyhow::anyhow!("Order processing timed out"));
        }
    }

    let cert = order
        .certificate()
        .await?
        .ok_or(anyhow::anyhow!("Certificate not ready despite Valid state"))?;
    save_cert_and_update_config(
        &cert,
        &private_key_pem,
        &domain,
        enable_https,
        enable_sip_tls,
        &app_state,
    )?;

    {
        let mut status = acme_state.status.write().unwrap();
        *status = AcmeStatus::Success(format!("Certificate for {} issued successfully", domain));
    }

    Ok(())
}

fn save_cert_and_update_config(
    cert: &str,
    private_key_pem: &str,
    domain: &str,
    enable_https: bool,
    enable_sip_tls: bool,
    app_state: &AppState,
) -> anyhow::Result<()> {
    let cert_dir = StdPath::new("config/certs");
    std::fs::create_dir_all(cert_dir)?;

    let cert_path = cert_dir.join(format!("{}.crt", domain));
    let key_path = cert_dir.join(format!("{}.key", domain));

    std::fs::write(&cert_path, cert)?;
    std::fs::write(&key_path, private_key_pem)?;

    info!("Certificate saved to {:?}", cert_path);

    if enable_https || enable_sip_tls {
        let config_path = app_state
            .config_path
            .clone()
            .unwrap_or_else(|| "config.toml".to_string());
        let config_content = std::fs::read_to_string(&config_path)?;
        let mut doc = config_content.parse::<DocumentMut>()?;

        if !doc.contains_key("proxy") {
            doc["proxy"] = toml_edit::table();
        }

        let proxy = &mut doc["proxy"];

        proxy["ssl_certificate"] = value(cert_path.to_string_lossy().to_string());
        proxy["ssl_private_key"] = value(key_path.to_string_lossy().to_string());

        if enable_sip_tls {
            if proxy.get("tls_port").is_none() {
                proxy["tls_port"] = value(5061);
            }
        }

        if enable_https {
            doc["ssl_certificate"] = value(cert_path.to_string_lossy().to_string());
            doc["ssl_private_key"] = value(key_path.to_string_lossy().to_string());
            if doc.get("https_addr").is_none() {
                doc["https_addr"] = value("0.0.0.0:443");
            }
        }

        std::fs::write(&config_path, doc.to_string())?;
        info!("Updated config.toml with new certificate paths");
    }
    Ok(())
}

async fn solve_http01_challenge<'a>(
    auth: &'a mut instant_acme::AuthorizationHandle<'a>,
    acme_state: &AcmeState,
) -> anyhow::Result<String> {
    use instant_acme::ChallengeType;
    let mut challenge = auth
        .challenge(ChallengeType::Http01)
        .ok_or(anyhow::anyhow!("No http-01 challenge found"))?;
    let token = challenge.token.to_string();
    let key_auth = challenge.key_authorization();
    {
        let mut challenges = acme_state.challenges.write().unwrap();
        challenges.insert(token.clone(), key_auth.as_str().to_string());
    }
    challenge.set_ready().await?;
    Ok(token)
}
