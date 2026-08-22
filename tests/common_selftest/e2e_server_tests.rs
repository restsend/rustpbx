//! Self-tests for `tests/common/e2e_test_server.rs`.
//! Extracted from the module's embedded `#[cfg(test)] mod tests` so the
//! harness self-tests run ONCE here instead of in every aggregator
//! binary (they used to re-execute ~11x per `cargo test` run).


use crate::common::e2e_test_server::*;

#[tokio::test]
async fn test_e2e_server_start() {
    let server = E2eTestServer::start().await;
    assert!(server.is_ok());

    let server = server.unwrap();
    assert!(server.port > 0);

    // Cleanup
    server.stop();
}

#[tokio::test]
async fn test_create_ua() {
    let server = E2eTestServer::start().await.unwrap();

    let ua = server.create_ua("alice").await;
    assert!(ua.is_ok());

    server.stop();
}
