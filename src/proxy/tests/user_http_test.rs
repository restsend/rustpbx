use crate::proxy::user::UserBackend;
use crate::proxy::user_http::HttpUserBackend;
use anyhow::Result;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_backend_creation() -> Result<()> {
    let _backend = HttpUserBackend::new(
        "http://rustpbx.com/auth",
        &Some("GET".to_string()),
        &Some("user".to_string()),
        &Some("realm".to_string()),
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
    );

    Ok(())
}

#[tokio::test]
async fn test_http_backend_with_custom_headers() -> Result<()> {
    let mut headers = HashMap::new();
    headers.insert("User-Agent".to_string(), "RustPBX-Test".to_string());
    headers.insert("X-Auth-Token".to_string(), "test-token".to_string());

    let _backend = HttpUserBackend::new(
        "http://rustpbx.com/auth",
        &Some("GET".to_string()),
        &Some("user".to_string()),
        &Some("realm".to_string()),
        &None,
        &Some(headers),
        &None,
        &None,
        &None,
        &None,
        &None,
    );

    Ok(())
}

#[tokio::test]
async fn test_http_backend_with_post_method() -> Result<()> {
    let _backend = HttpUserBackend::new(
        "http://rustpbx.com/auth",
        &Some("POST".to_string()),
        &Some("username".to_string()),
        &Some("domain".to_string()),
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
    );

    Ok(())
}

#[tokio::test]
async fn test_http_backend_with_defaults() -> Result<()> {
    let _backend = HttpUserBackend::new(
        "http://rustpbx.com/auth",
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
    );

    Ok(())
}

#[tokio::test]
async fn test_http_backend_get_user() -> Result<()> {
    let backend = HttpUserBackend::new(
        "http://httpbin.org/json",
        &Some("GET".to_string()),
        &Some("username".to_string()),
        &Some("realm".to_string()),
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
        &None,
    );

    let result = backend
        .get_user("testuser", Some("rustpbx.com"), None)
        .await;

    assert!(result.is_err());

    Ok(())
}
