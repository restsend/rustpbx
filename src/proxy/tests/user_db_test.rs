use crate::proxy::user::UserBackend;
use crate::proxy::user_db::{DbBackend, DbBackendConfig};
use tempfile;

#[tokio::test]
async fn test_db_backend() {
    // Create a temporary file for the database
    let temp_db_file = tempfile::NamedTempFile::new().unwrap();
    let db_url = format!("sqlite:{}", temp_db_file.path().to_str().unwrap());

    sqlx::any::install_default_drivers();
    // Setup the database directly with SQLite
    let setup_db = sqlx::SqlitePool::connect(&db_url).await.unwrap();

    // Create test table
    sqlx::query(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            username TEXT NOT NULL,
            password TEXT NOT NULL,
            enabled INTEGER DEFAULT 1,
            realm TEXT
        )",
    )
    .execute(&setup_db)
    .await
    .unwrap();

    // Insert test data
    sqlx::query("INSERT INTO users (username, password, realm) VALUES (?, ?, ?), (?, ?, ?)")
        .bind("testuser")
        .bind("testpass")
        .bind("rustpbx.com")
        .bind("admin")
        .bind("adminpass")
        .bind("rustpbx.com")
        .execute(&setup_db)
        .await
        .unwrap();

    // Create custom users table for testing optional columns
    sqlx::query(
        "CREATE TABLE custom_users (
                id INTEGER PRIMARY KEY,
                user_name TEXT NOT NULL,
                pass_word TEXT NOT NULL,
                is_enabled INTEGER DEFAULT 1,
                realm_name TEXT
            )",
    )
    .execute(&setup_db)
    .await
    .unwrap();

    sqlx::query("INSERT INTO custom_users (user_name, pass_word, is_enabled, realm_name) VALUES (?, ?, ?, ?)")
        .bind("customuser")
        .bind("custompass")
        .bind(1)
        .bind("rustpbx.com")
        .execute(&setup_db)
        .await
        .unwrap();

    // Close the setup connection
    setup_db.close().await;
    let db_config = DbBackendConfig {
        table_name: "users".to_string(),
        username_column: "username".to_string(),
        password_column: "password".to_string(),
        enabled_column: Some("enabled".to_string()),
        id_column: Some("id".to_string()),
        realm_column: Some("realm".to_string()),
        ..Default::default()
    };
    // Create backend with the database URL (this will create its own connection)
    let backend = DbBackend::new(db_url.clone(), db_config, None)
        .await
        .expect("Failed to create DbBackend");

    // Test get_user
    let user = backend
        .get_user("testuser", None, None)
        .await
        .unwrap()
        .expect("Failed to get user");
    assert_eq!(user.username, "testuser");
    assert_eq!(user.password, Some("testpass".to_string()));
    assert!(user.enabled);

    // Test another user
    let admin_user = backend
        .get_user("admin", None, None)
        .await
        .unwrap()
        .expect("Failed to get user");
    assert_eq!(admin_user.username, "admin");
    assert_eq!(admin_user.password, Some("adminpass".to_string()));
    assert!(admin_user.enabled);

    // Test with custom table and column names
    let db_config = DbBackendConfig {
        table_name: "custom_users".to_string(),
        username_column: "user_name".to_string(),
        password_column: "pass_word".to_string(),
        enabled_column: Some("is_enabled".to_string()),
        id_column: Some("id".to_string()),
        realm_column: Some("realm_name".to_string()),
        ..Default::default()
    };
    let custom_backend = DbBackend::new(db_url.clone(), db_config, None)
        .await
        .expect("Failed to create DbBackend");

    // Test get_user with custom table
    let user = custom_backend
        .get_user("customuser", None, None)
        .await
        .unwrap()
        .expect("Failed to get user");
    assert_eq!(user.username, "customuser");
    assert_eq!(user.password, Some("custompass".to_string()));
    assert!(user.enabled);

    // Test with realm filtering
    let user_with_realm = custom_backend
        .get_user("customuser", Some("rustpbx.com"), None)
        .await
        .unwrap()
        .expect("Failed to get user");
    assert_eq!(user_with_realm.username, "customuser");
    assert_eq!(user_with_realm.realm, Some("rustpbx.com".to_string()));
}

/// Runs the backend's lookups against `url` using a throwaway `table`:
/// user found with its password, realm matched (also as `host:port`),
/// unknown user and realm rejected.
async fn assert_db_backend_lookups(url: &str, table: &str) {
    use sqlx::Executor;
    sqlx::any::install_default_drivers();
    let setup = sqlx::AnyPool::connect(url).await.unwrap();
    setup.execute(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}"))).await.unwrap();
    setup
        .execute(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {table} (id BIGINT PRIMARY KEY, username VARCHAR(64) NOT NULL, \
             password VARCHAR(64) NOT NULL, realm VARCHAR(64))"
        )))
        .await
        .unwrap();
    setup
        .execute(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {table} (id, username, password, realm) VALUES \
             (1, 'alice', 'alicepass', 'rustpbx.com'), (2, 'bob', 'bobpass', 'other.com')"
        )))
        .await
        .unwrap();
    let config = DbBackendConfig {
        table_name: table.to_string(),
        id_column: Some("id".to_string()),
        realm_column: Some("realm".to_string()),
        ..Default::default()
    };
    let backend = DbBackend::new(url.to_string(), config, None).await.unwrap();

    let alice = backend.get_user("alice", None, None).await.unwrap().expect("alice");
    assert_eq!((alice.id, alice.password.as_deref()), (1, Some("alicepass")));
    let alice = backend
        .get_user("alice", Some("rustpbx.com:5060"), None)
        .await
        .unwrap()
        .expect("alice by host:port");
    assert_eq!(alice.username, "alice");
    assert!(backend.get_user("alice", Some("other.com"), None).await.unwrap().is_none());
    assert!(backend.get_user("nobody", None, None).await.unwrap().is_none());
    assert!(backend.is_same_realm("rustpbx.com:5060").await);
    assert!(!backend.is_same_realm("unknown.com").await);

    setup.execute(sqlx::AssertSqlSafe(format!("DROP TABLE {table}"))).await.unwrap();
}

#[tokio::test]
async fn test_db_backend_lookups_sqlite() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let url = format!("sqlite:{}", file.path().to_str().unwrap());
    assert_db_backend_lookups(&url, "sip_users").await;
}

#[tokio::test]
#[ignore = "requires RUSTPBX_POSTGRES_TEST_URL pointing to a PostgreSQL test database"]
async fn test_db_backend_lookups_postgres() {
    let url = std::env::var("RUSTPBX_POSTGRES_TEST_URL").unwrap();
    assert_db_backend_lookups(&url, "rustpbx_user_db_test").await;
}

#[tokio::test]
#[ignore = "requires RUSTPBX_MYSQL_TEST_URL pointing to a MySQL test server"]
async fn test_db_backend_lookups_mysql() {
    let url = std::env::var("RUSTPBX_MYSQL_TEST_URL").unwrap();
    assert_db_backend_lookups(&url, "rustpbx_user_db_test").await;
}
