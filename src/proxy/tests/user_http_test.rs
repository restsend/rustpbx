use crate::proxy::user::UserBackend;
use crate::proxy::user_http::HttpUserBackend;
use anyhow::Result;
use axum::{
    extract::{Form, Query},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashMap as SerdeHashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Deserialize, Serialize)]
struct AuthRequest {
    user: Option<String>,
    pass: Option<String>,
}

#[derive(Debug, Serialize)]
struct JsonResponse {
    success: bool,
}

async fn handle_auth_get(Query(params): Query<SerdeHashMap<String, String>>) -> impl IntoResponse {
    let username = params.get("user").unwrap_or(&"".to_string()).clone();
    let password = params.get("pass").unwrap_or(&"".to_string()).clone();

    if username == "testuser" && password == "testpass" {
        axum::Json(JsonResponse { success: true })
    } else {
        axum::Json(JsonResponse { success: false })
    }
}

async fn handle_auth_post(Form(params): Form<SerdeHashMap<String, String>>) -> impl IntoResponse {
    let username = params.get("user").unwrap_or(&"".to_string()).clone();
    let password = params.get("pass").unwrap_or(&"".to_string()).clone();

    if username == "testuser" && password == "testpass" {
        axum::Json(JsonResponse { success: true })
    } else {
        axum::Json(JsonResponse { success: false })
    }
}

async fn handle_auth_status(Query(params): Query<SerdeHashMap<String, String>>) -> StatusCode {
    let username = params.get("user").unwrap_or(&"".to_string()).clone();
    let password = params.get("pass").unwrap_or(&"".to_string()).clone();

    if username == "testuser" && password == "testpass" {
        StatusCode::OK
    } else {
        StatusCode::UNAUTHORIZED
    }
}

async fn start_test_server() -> SocketAddr {
    let app = Router::new()
        .route("/auth/json", get(handle_auth_get).post(handle_auth_post))
        .route("/auth/status", get(handle_auth_status));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    addr
}

#[tokio::test]
async fn test_http_auth_get_with_json_response() -> Result<()> {
    let addr = start_test_server().await;
    let url = format!("http://{}/auth/json", addr);

    let backend = HttpUserBackend::new(
        &url,
        Some("GET".to_string()),
        Some("user".to_string()),
        Some("pass".to_string()),
        None,
    );

    // Test valid credentials
    let result = backend.authenticate("testuser", "testpass").await?;
    assert!(result);

    // Test invalid credentials
    let result = backend.authenticate("testuser", "wrongpass").await?;
    assert!(!result);

    // Test non-existent user
    let result = backend.authenticate("nonexistent", "password").await?;
    assert!(!result);

    Ok(())
}

#[tokio::test]
async fn test_http_auth_post_with_json_response() -> Result<()> {
    let addr = start_test_server().await;
    let url = format!("http://{}/auth/json", addr);

    let backend = HttpUserBackend::new(
        &url,
        Some("POST".to_string()),
        Some("user".to_string()),
        Some("pass".to_string()),
        None,
    );

    // Test valid credentials
    let result = backend.authenticate("testuser", "testpass").await?;
    assert!(result);

    // Test invalid credentials
    let result = backend.authenticate("testuser", "wrongpass").await?;
    assert!(!result);

    // Test non-existent user
    let result = backend.authenticate("nonexistent", "password").await?;
    assert!(!result);

    Ok(())
}

#[tokio::test]
async fn test_http_auth_with_status_response() -> Result<()> {
    let addr = start_test_server().await;
    let url = format!("http://{}/auth/status", addr);

    let backend = HttpUserBackend::new(
        &url,
        Some("GET".to_string()),
        Some("user".to_string()),
        Some("pass".to_string()),
        None,
    );

    // Test valid credentials
    let result = backend.authenticate("testuser", "testpass").await?;
    assert!(result);

    // Test invalid credentials
    let result = backend.authenticate("testuser", "wrongpass").await?;
    assert!(!result);

    Ok(())
}

#[tokio::test]
async fn test_http_auth_with_custom_headers() -> Result<()> {
    let addr = start_test_server().await;
    let url = format!("http://{}/auth/json", addr);

    let mut headers = HashMap::new();
    headers.insert("User-Agent".to_string(), "RustPBX-Test".to_string());
    headers.insert("X-Auth-Token".to_string(), "test-token".to_string());

    let backend = HttpUserBackend::new(
        &url,
        Some("GET".to_string()),
        Some("user".to_string()),
        Some("pass".to_string()),
        Some(headers),
    );

    // Test valid credentials
    let result = backend.authenticate("testuser", "testpass").await?;
    assert!(result);

    Ok(())
}

#[tokio::test]
async fn test_get_user() -> Result<()> {
    let addr = start_test_server().await;
    let url = format!("http://{}/auth/json", addr);

    let backend = HttpUserBackend::new(
        &url,
        Some("GET".to_string()),
        Some("user".to_string()),
        Some("pass".to_string()),
        None,
    );

    // Test getting an existing user
    let user = backend.get_user("testuser", Some("example.com")).await?;
    assert_eq!(user.username, "testuser");
    assert_eq!(user.realm, Some("example.com".to_string()));
    assert!(user.enabled);

    Ok(())
}
