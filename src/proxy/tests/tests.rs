use super::*;
use std::io::Write;
use tempfile::NamedTempFile;
use tokio::runtime::Runtime;

#[test]
fn test_plaintext_backend() {
    // Create a temporary file with test users
    let mut temp_file = NamedTempFile::new().unwrap();
    writeln!(temp_file, "user1:password1").unwrap();
    writeln!(temp_file, "user2:password2").unwrap();
    writeln!(temp_file, "# Comment line").unwrap();
    writeln!(temp_file, "").unwrap();
    writeln!(temp_file, "invalid_line").unwrap();
    let path = temp_file.path().to_str().unwrap().to_string();

    // Create runtime for async tests
    let rt = Runtime::new().unwrap();

    // Test authentication
    rt.block_on(async {
        let backend = PlainTextBackend::new(&path);
        backend.load().await.unwrap();

        // Valid credentials
        assert!(backend.authenticate("user1", "password1").await.unwrap());
        assert!(backend.authenticate("user2", "password2").await.unwrap());

        // Invalid credentials
        assert!(!backend.authenticate("user1", "wrong").await.unwrap());
        assert!(!backend
            .authenticate("nonexistent", "password")
            .await
            .unwrap());
    });
}

async fn setup_test_db() -> (DatabaseConnection, String) {
    // To run this test, you need a real PostgreSQL database
    // This uses the DATABASE_URL environment variable
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL environment variable must be set for tests");

    // Create a random suffix for the test database
    let suffix = format!(
        "_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    let test_db_name = format!("test_rustpbx{}", suffix);

    // Create test database using sea-orm
    let db = Database::connect(&database_url).await.unwrap();

    // Create test database
    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        format!("CREATE DATABASE {}", test_db_name),
    );
    db.execute(stmt).await.unwrap();

    // Connect to test database
    let test_db_url = database_url.replace(&database_url.split('/').last().unwrap(), &test_db_name);
    let test_db = Database::connect(&test_db_url).await.unwrap();

    // Create test table
    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        "CREATE TABLE users (
            id SERIAL PRIMARY KEY,
            username VARCHAR(255) NOT NULL,
            password VARCHAR(255) NOT NULL
        )",
    );
    test_db.execute(stmt).await.unwrap();

    // Insert test data
    let stmt = Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "INSERT INTO users (username, password) VALUES ($1, $2), ($3, $4)",
        [
            "testuser".into(),
            "testpass".into(),
            "admin".into(),
            "adminpass".into(),
        ],
    );
    test_db.execute(stmt).await.unwrap();

    (db, test_db_url)
}

async fn cleanup_test_db(db: DatabaseConnection, test_db_name: &str) {
    // Drop connections and remove test database
    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{}'",
            test_db_name
        ),
    );
    db.execute(stmt).await.unwrap();

    let stmt = Statement::from_string(
        DatabaseBackend::Postgres,
        format!("DROP DATABASE IF EXISTS {}", test_db_name),
    );
    db.execute(stmt).await.unwrap();
}

#[test]
#[ignore] // Ignore by default as it requires a PostgreSQL database
fn test_db_backend() {
    // Check if we should run the test
    if std::env::var("RUN_DB_TESTS").is_err() {
        println!("Skipping database tests. Set RUN_DB_TESTS env var to run them.");
        return;
    }

    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        // Setup test database
        let (db, test_db_url) = setup_test_db().await;
        let test_db_name = test_db_url.split('/').last().unwrap().to_string();

        // Create backend
        let backend = DbBackend::new(test_db_url.clone(), None, None, None, None, None, None)
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

        // Test with custom table and column names
        let stmt = Statement::from_string(
            DatabaseBackend::Postgres,
            "CREATE TABLE custom_users (
                id SERIAL PRIMARY KEY,
                user_name VARCHAR(255) NOT NULL,
                pass_word VARCHAR(255) NOT NULL
            )",
        );
        backend.db.execute(stmt).await.unwrap();

        let stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO custom_users (user_name, pass_word) VALUES ($1, $2)",
            ["customuser".into(), "custompass".into()],
        );
        backend.db.execute(stmt).await.unwrap();

        let custom_backend = DbBackend::new(
            test_db_url,
            Some("custom_users".to_string()),
            Some("user_name".to_string()),
            Some("pass_word".to_string()),
            Some("enabled".to_string()),
            Some("sha256".to_string()),
            Some("".to_string()),
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

        // Cleanup
        cleanup_test_db(db, &test_db_name).await;
    });
}

#[test]
fn test_create_user_backend() {
    // Create a temporary file with test users
    let mut temp_file = NamedTempFile::new().unwrap();
    writeln!(temp_file, "user1:password1").unwrap();
    let path = temp_file.path().to_str().unwrap().to_string();

    let rt = Runtime::new().unwrap();

    rt.block_on(async {
        // Test with plain text config
        let plain_config = UserBackendConfig::Plain { path: path.clone() };

        let backend = create_user_backend(&plain_config).await.unwrap();
        assert!(backend.is_some());
        let backend = backend.unwrap();
        assert!(backend.authenticate("user1", "password1").await.unwrap());
        assert!(!backend.authenticate("user1", "wrong").await.unwrap());
        let plain_config = UserBackendConfig::default();
        // Test with None config
        let backend = create_user_backend(&plain_config).await.unwrap();
        assert!(backend.is_none());
    });
}
