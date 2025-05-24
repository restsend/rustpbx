use crate::proxy::user::UserBackend;
use crate::proxy::user_db::DbBackend;
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
        .bind("example.com")
        .bind("admin")
        .bind("adminpass")
        .bind("example.com")
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
        .bind("example.com")
        .execute(&setup_db)
        .await
        .unwrap();

    // Close the setup connection
    setup_db.close().await;

    // Create backend with the database URL (this will create its own connection)
    let backend = DbBackend::new(db_url.clone(), None, None, None, None, None, None)
        .await
        .unwrap();

    // Test get_user
    let user = backend.get_user("testuser", None).await.unwrap();
    assert_eq!(user.username, "testuser");
    assert_eq!(user.password, Some("testpass".to_string()));
    assert!(user.enabled);

    // Test another user
    let admin_user = backend.get_user("admin", None).await.unwrap();
    assert_eq!(admin_user.username, "admin");
    assert_eq!(admin_user.password, Some("adminpass".to_string()));
    assert!(admin_user.enabled);

    // Test with custom table and column names
    let custom_backend = DbBackend::new(
        db_url.clone(),
        Some("custom_users".to_string()),
        Some("id".to_string()),
        Some("user_name".to_string()),
        Some("pass_word".to_string()),
        Some("is_enabled".to_string()),
        Some("realm_name".to_string()),
    )
    .await
    .unwrap();

    // Test get_user with custom table
    let user = custom_backend.get_user("customuser", None).await.unwrap();
    assert_eq!(user.username, "customuser");
    assert_eq!(user.password, Some("custompass".to_string()));
    assert!(user.enabled);

    // Test with realm filtering
    let user_with_realm = custom_backend
        .get_user("customuser", Some("example.com"))
        .await
        .unwrap();
    assert_eq!(user_with_realm.username, "customuser");
    assert_eq!(user_with_realm.realm, Some("example.com".to_string()));
}
