use crate::{app::AppState, handler::middleware::clientaddr::ClientAddr};
use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use tracing::warn;

#[cfg(feature = "console")]
use crate::console::middleware::extract_session_cookie;

pub async fn ami_auth_middleware(
    State(state): State<AppState>,
    client_ip: ClientAddr,
    request: Request,
    next: Next,
) -> Response {
    #[allow(unused_mut)]
    let mut allowed = state.config().ami.as_ref().map_or(false, |ami| {
        ami.is_allowed(client_ip.ip().to_string().as_str())
    });

    #[cfg(feature = "console")]
    if !allowed {
        // Let authenticated console superusers bypass AMI IP checks.
        if let Some(console_state) = &state.console {
            if let Some(cookie_value) = extract_session_cookie(request.headers()) {
                match console_state.current_user(Some(&cookie_value)).await {
                    Ok(Some(user)) => {
                        if user.is_superuser
                            || console_state.has_permission(&user, "ami", "access").await
                        {
                            allowed = true;
                        }
                    }
                    Ok(None) => {
                        // Session cookie present but no active user; fall back to IP checks
                    }
                    Err(err) => {
                        warn!(%client_ip, error = %err, "Failed to resolve console user for AMI access");
                    }
                }
            }
        }
    }

    if !allowed {
        warn!(
            %client_ip,
            "AMI access denied for client"
        );

        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "Access denied",
                "message": "You don't have permission to access AMI interfaces"
            })),
        )
            .into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use crate::config::AmiConfig;

    #[test]
    fn ami_config_allows_localhost_by_default() {
        let cfg = AmiConfig::default();
        assert!(cfg.is_allowed("127.0.0.1"));
        assert!(cfg.is_allowed("::1"));
        assert!(cfg.is_allowed("localhost"));
    }

    #[test]
    fn ami_config_denies_remote_ip_by_default() {
        let cfg = AmiConfig::default();
        assert!(!cfg.is_allowed("203.0.113.42"));
        assert!(!cfg.is_allowed("10.0.0.1"));
    }

    #[test]
    fn ami_config_wildcard_allows_any_ip() {
        let cfg = AmiConfig {
            allows: Some(vec!["*".into()]),
        };
        assert!(cfg.is_allowed("203.0.113.42"));
        assert!(cfg.is_allowed("127.0.0.1"));
        assert!(cfg.is_allowed("10.0.0.1"));
    }

    #[test]
    fn ami_config_explicit_list_allows_only_listed() {
        let cfg = AmiConfig {
            allows: Some(vec!["10.0.0.1".into(), "192.168.1.100".into()]),
        };
        assert!(cfg.is_allowed("10.0.0.1"));
        assert!(cfg.is_allowed("192.168.1.100"));
        assert!(!cfg.is_allowed("127.0.0.1"));
        assert!(!cfg.is_allowed("203.0.113.42"));
    }

    #[test]
    fn ami_config_empty_allows_list_denies_all() {
        let cfg = AmiConfig {
            allows: Some(vec![]),
        };
        assert!(!cfg.is_allowed("127.0.0.1"));
        assert!(!cfg.is_allowed("::1"));
        assert!(!cfg.is_allowed("10.0.0.1"));
    }
}
