use crate::proxy::user::UserBackend;
use crate::proxy::user_db::DbBackend;
use md5::Md5;
use sha1::Digest as Sha1Digest;
use sqlx::SqlitePool;
use tempfile;

async fn setup_test_db() -> SqlitePool {
    // Use SQLite in-memory database for testing
    let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();

    // Create test table
    sqlx::query(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            username TEXT NOT NULL,
            password TEXT NOT NULL,
            enabled INTEGER DEFAULT 1,
            realm TEXT NOT NULL,
        )",
    )
    .execute(&db)
    .await
    .unwrap();

    // Insert test data
    sqlx::query("INSERT INTO users (username, password) VALUES (?, ?), (?, ?)")
        .bind("testuser")
        .bind("testpass")
        .bind("admin")
        .bind("adminpass")
        .execute(&db)
        .await
        .unwrap();

    db
}

async fn create_hashed_password_test_data(db_pool: &SqlitePool, table_name: &str, hash_type: &str) {
    // Create test table with hashed passwords
    sqlx::query(&format!(
        "CREATE TABLE {} (
            id INTEGER PRIMARY KEY,
            username TEXT NOT NULL,
            password TEXT NOT NULL,
            enabled INTEGER DEFAULT 1
        )",
        table_name
    ))
    .execute(db_pool)
    .await
    .unwrap();

    // Insert test data with hashed passwords
    match hash_type {
        "md5" => {
            sqlx::query(&format!(
                "INSERT INTO {} (username, password) VALUES (?, ?)",
                table_name
            ))
            .bind("hashuser")
            .bind(format!(
                "{:x}",
                Md5::new().chain_update(b"hashpass").finalize()
            ))
            .execute(db_pool)
            .await
            .unwrap();
        }
        "md5_salt" => {
            sqlx::query(&format!(
                "INSERT INTO {} (username, password) VALUES (?, ?)",
                table_name
            ))
            .bind("hashuser")
            .bind(format!(
                "{:x}",
                Md5::new().chain_update(b"hashpasssalt123").finalize()
            ))
            .execute(db_pool)
            .await
            .unwrap();
        }
        "sha1" => {
            let mut hasher = sha1::Sha1::new();
            hasher.update(b"hashpass");
            let hash = hasher.finalize();

            sqlx::query(&format!(
                "INSERT INTO {} (username, password) VALUES (?, ?)",
                table_name
            ))
            .bind("hashuser")
            .bind(format!("{:x}", hash))
            .execute(db_pool)
            .await
            .unwrap();
        }
        "sha256" => {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(b"hashpass");
            let hash = hasher.finalize();

            sqlx::query(&format!(
                "INSERT INTO {} (username, password) VALUES (?, ?)",
                table_name
            ))
            .bind("hashuser")
            .bind(format!("{:x}", hash))
            .execute(db_pool)
            .await
            .unwrap();
        }
        "sha512" => {
            use sha2::{Digest, Sha512};
            let mut hasher = Sha512::new();
            hasher.update(b"hashpass");
            let hash = hasher.finalize();

            sqlx::query(&format!(
                "INSERT INTO {} (username, password) VALUES (?, ?)",
                table_name
            ))
            .bind("hashuser")
            .bind(format!("{:x}", hash))
            .execute(db_pool)
            .await
            .unwrap();
        }
        _ => {
            sqlx::query(&format!(
                "INSERT INTO {} (username, password) VALUES (?, ?)",
                table_name
            ))
            .bind("hashuser")
            .bind("hashpass")
            .execute(db_pool)
            .await
            .unwrap();
        }
    }
}

#[tokio::test]
#[ignore]
async fn test_db_backend() {
    // Create a temporary file for the database
    let temp_db_file = tempfile::NamedTempFile::new().unwrap();
    let db_url = format!("sqlite:{}", temp_db_file.path().to_str().unwrap());

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
