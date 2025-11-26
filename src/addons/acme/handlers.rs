use axum::{
    extract::{State, Path, Json},
    response::{Html, IntoResponse},
    Extension,
};
use crate::app::AppState;
use super::AcmeState;
use serde::Deserialize;
use tracing::{info, error};
use toml_edit::{DocumentMut, value};
use std::path::Path as StdPath;

#[derive(Deserialize)]
pub struct RequestCertPayload {
    domain: String,
    email: String,
    enable_https: bool,
    enable_sip_tls: bool,
}

pub async fn ui_index(State(state): State<AppState>) -> impl IntoResponse {
    #[cfg(feature = "console")]
    {
        if let Some(console) = &state.console {
            return console.render("acme/acme_index.html", serde_json::json!({}));
        }
    }

    Html("Console feature not enabled".to_string()).into_response()
}

pub async fn challenge(
    Path(token): Path<String>,
    Extension(acme_state): Extension<AcmeState>,
) -> impl IntoResponse {
    let challenges = acme_state.challenges.read().unwrap();
    if let Some(response) = challenges.get(&token) {
        response.clone()
    } else {
        "Not Found".to_string()
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
    
    tokio::spawn(async move {
        if let Err(e) = process_acme(domain, email, enable_https, enable_sip_tls, acme_state, state).await {
            error!("ACME processing failed: {}", e);
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
    app_state: AppState
) -> anyhow::Result<()> {
    use instant_acme::{Account, NewAccount, Identifier, AuthorizationStatus, OrderStatus, NewOrder};
    
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
        ).await?;

    let mut order = account.new_order(&NewOrder::new(&[Identifier::Dns(domain.clone())])).await?;

    loop {
        let mut pending_auth_url = None;
        let mut token = String::new();
        
        {
            let mut authorizations = order.authorizations();
            while let Some(auth_result) = authorizations.next().await {
                let mut auth = auth_result?;
                if auth.status == AuthorizationStatus::Pending {
                    pending_auth_url = Some(auth.url().to_string());
                    token = solve_http01_challenge(&mut auth, &acme_state).await?;
                    break; 
                }
            }
        }
        
        if let Some(url) = pending_auth_url {
            let mut retries = 0;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                
                let mut authorizations = order.authorizations();
                let mut status = AuthorizationStatus::Pending;
                let mut found = false;
                
                while let Some(auth_res) = authorizations.next().await {
                    let auth = auth_res?;
                    if auth.url() == url {
                        status = auth.status;
                        found = true;
                        break;
                    }
                }
                
                if !found {
                    return Err(anyhow::anyhow!("Authorization not found in order"));
                }
                
                if status == AuthorizationStatus::Valid {
                    break;
                }
                if status == AuthorizationStatus::Invalid {
                    return Err(anyhow::anyhow!("Authorization invalid"));
                }
                
                retries += 1;
                if retries > 30 {
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

    let params = rcgen::CertificateParams::new(vec![domain.clone()])?;
    let key_pair = rcgen::KeyPair::generate()?;
    let private_key_pem = key_pair.serialize_pem();
    let csr = params.serialize_request(&key_pair)?;

    let state = order.refresh().await?;
    if state.status == OrderStatus::Ready {
        order.finalize_csr(&csr.der()).await?;
    }

    let mut retries = 0;
    loop {
        let state = order.state();
        if state.status == OrderStatus::Valid {
            break;
        }
        if state.status == OrderStatus::Invalid {
             return Err(anyhow::anyhow!("Order invalid"));
        }
        
        if let Some(cert) = order.certificate().await? {
            save_cert_and_update_config(&cert, &private_key_pem, &domain, enable_https, enable_sip_tls, &app_state)?;
            return Ok(());
        }
        
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        order.refresh().await?;
        
        retries += 1;
        if retries > 30 {
             return Err(anyhow::anyhow!("Order processing timed out"));
        }
    }
    
    let cert = order.certificate().await?.ok_or(anyhow::anyhow!("Certificate not ready despite Valid state"))?;
    save_cert_and_update_config(&cert, &private_key_pem, &domain, enable_https, enable_sip_tls, &app_state)?;

    Ok(())
}

fn save_cert_and_update_config(
    cert: &str, 
    private_key_pem: &str, 
    domain: &str, 
    enable_https: bool, 
    enable_sip_tls: bool, 
    app_state: &AppState
) -> anyhow::Result<()> {
    let cert_dir = StdPath::new("config/certs");
    std::fs::create_dir_all(cert_dir)?;
    
    let cert_path = cert_dir.join(format!("{}.crt", domain));
    let key_path = cert_dir.join(format!("{}.key", domain));
    
    std::fs::write(&cert_path, cert)?;
    std::fs::write(&key_path, private_key_pem)?;
    
    info!("Certificate saved to {:?}", cert_path);
    
    if enable_https || enable_sip_tls {
        let config_path = app_state.config_path.clone().unwrap_or_else(|| "config.toml".to_string());
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
        
        std::fs::write(&config_path, doc.to_string())?;
        info!("Updated config.toml with new certificate paths");
    }
    Ok(())
}

async fn solve_http01_challenge<'a>(
    auth: &'a mut instant_acme::AuthorizationHandle<'a>,
    acme_state: &AcmeState
) -> anyhow::Result<String> {
    use instant_acme::ChallengeType;
    let mut challenge = auth.challenge(ChallengeType::Http01).ok_or(anyhow::anyhow!("No http-01 challenge found"))?;
    let token = challenge.token.to_string();
    let key_auth = challenge.key_authorization();
    {
        let mut challenges = acme_state.challenges.write().unwrap();
        challenges.insert(token.clone(), key_auth.as_str().to_string());
    }
    challenge.set_ready().await?;
    Ok(token)
}

