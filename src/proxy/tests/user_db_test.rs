use crate::proxy::user::UserBackend;
use crate::proxy::user_db::DbBackend;
use sha1::Digest as Sha1Digest;
use sqlx::AnyPool;

async fn setup_test_db() -> AnyPool {
    // Use SQLite in-memory database for testing
    let db = sqlx::any::AnyPoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    // Create test table
    sqlx::query(
        "CREATE TABLE users (
            id INTEGER PRIMARY KEY,
            username TEXT NOT NULL,
            password TEXT NOT NULL,
            enabled INTEGER DEFAULT 1
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

async fn create_hashed_password_test_data(db_pool: &AnyPool, table_name: &str, hash_type: &str) {
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
            .bind(format!("{:x}", md5::compute("hashpass")))
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
            .bind(format!("{:x}", md5::compute("hashpasssalt123")))
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
    // Setup test database
    let db = setup_test_db().await;

    // Create backend with in-memory SQLite
    let backend = DbBackend::new(
        "sqlite::memory:".to_string(),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Test authentication
    assert!(backend.authenticate("testuser", "testpass").await.unwrap());
    assert!(backend.authenticate("admin", "adminpass").await.unwrap());
    assert!(!backend.authenticate("testuser", "wrongpass").await.unwrap());
    assert!(!backend
        .authenticate("nonexistent", "password")
        .await
        .unwrap());

    // Test get_user
    let user = backend.get_user("testuser", None).await.unwrap();
    assert_eq!(user.username, "testuser");
    assert_eq!(user.password, Some("testpass".to_string()));
    assert!(user.enabled);

    // Create a separate connection for the custom table tests
    let test_db = sqlx::any::AnyPoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    // Test with custom table and column names
    sqlx::query(
        "CREATE TABLE custom_users (
                id INTEGER PRIMARY KEY,
                user_name TEXT NOT NULL,
                pass_word TEXT NOT NULL,
                is_enabled INTEGER DEFAULT 1
            )",
    )
    .execute(&test_db)
    .await
    .unwrap();

    sqlx::query("INSERT INTO custom_users (user_name, pass_word, is_enabled) VALUES (?, ?, ?)")
        .bind("customuser")
        .bind("custompass")
        .bind(1)
        .execute(&test_db)
        .await
        .unwrap();

    let custom_backend = DbBackend::new(
        "sqlite::memory:".to_string(),
        Some("custom_users".to_string()),
        Some("user_name".to_string()),
        Some("pass_word".to_string()),
        Some("is_enabled".to_string()),
        None,
        None,
    )
    .await
    .unwrap();

    assert!(custom_backend
        .authenticate("customuser", "custompass")
        .await
        .unwrap());
    assert!(!custom_backend
        .authenticate("customuser", "wrongpass")
        .await
        .unwrap());

    // Test get_user with custom table
    let user = custom_backend.get_user("customuser", None).await.unwrap();
    assert_eq!(user.username, "customuser");
    assert_eq!(user.password, Some("custompass".to_string()));
    assert!(user.enabled);

    // Test with different hash algorithms
    for (hash_type, table_name) in [
        ("md5", "md5_users"),
        ("sha1", "sha1_users"),
        ("sha256", "sha256_users"),
        ("sha512", "sha512_users"),
        ("md5_salt", "md5_salt_users"),
    ] {
        let salt = if hash_type == "md5_salt" {
            "salt123"
        } else {
            ""
        };

        // Create a new connection for each hash test
        let hash_db = sqlx::any::AnyPoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();

        // Create table with hashed password
        create_hashed_password_test_data(
            &hash_db,
            table_name,
            hash_type.replace("_salt", "").as_str(),
        )
        .await;

        // Create backend with hash algorithm
        let hash_backend = DbBackend::new(
            "sqlite::memory:".to_string(),
            Some(table_name.to_string()),
            None,
            None,
            None,
            Some(hash_type.replace("_salt", "").to_string()),
            Some(salt.to_string()),
        )
        .await
        .unwrap();

        // Test authentication with hash
        assert!(hash_backend
            .authenticate("hashuser", "hashpass")
            .await
            .unwrap());
        assert!(!hash_backend
            .authenticate("hashuser", "wrongpass")
            .await
            .unwrap());
    }
}
