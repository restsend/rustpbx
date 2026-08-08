use crate::proxy::user::UserBackend;
use crate::proxy::user_http::HttpUserBackend;
use anyhow::Result;

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
