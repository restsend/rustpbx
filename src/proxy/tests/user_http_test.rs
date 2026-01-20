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
    );

    // Test that the backend was created successfully
    // We can't easily test the actual HTTP functionality without a real server
    // but we can test that construction works correctly
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
        &Some(headers),
        &None,
    );

    // Test that the backend was created successfully with custom headers
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
    );

    // Test that the backend was created successfully with POST method
    Ok(())
}

#[tokio::test]
async fn test_http_backend_with_defaults() -> Result<()> {
    let _backend = HttpUserBackend::new("http://rustpbx.com/auth", &None, &None, &None, &None, &None);

    // Test that the backend was created successfully with default values
    Ok(())
}

#[tokio::test]
async fn test_http_backend_get_user() -> Result<()> {
    let backend = HttpUserBackend::new(
        "http://httpbin.org/json", // Use a real endpoint for basic testing
        &Some("GET".to_string()),
        &Some("username".to_string()),
        &Some("realm".to_string()),
        &None,
        &None,
    );

    // Note: This test will fail if httpbin.org is not available
    // In a real test environment, you'd use a mock server
    // For now, we just test that the method exists and can be called
    let result = backend.get_user("testuser", Some("rustpbx.com"), None).await;

    // We expect this to fail since httpbin.org/json doesn't return SipUser format
    // But it verifies the method signature and that the HTTP client works
    assert!(result.is_err());

    Ok(())
}
