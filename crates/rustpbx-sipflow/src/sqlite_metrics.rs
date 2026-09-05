//! General-purpose sqlite instrumentation.
//!
//! Complements the domain-specific `sipflow_*` metrics with a `sqlite_*`
//! taxonomy so the same questions (connections, statements, transactions,
//! WAL health) can be asked of any sqlite usage in this binary. Every
//! metric carries a `database` label; sipflow always reports `sipflow`.
//! Statement-level only: multi-row/batched operations, never per-row.

use std::path::Path;
use std::time::{Duration, Instant};
use sqlx::{Connection, SqliteConnection};

pub const DATABASE: &str = "sipflow";

/// Classify a SQL statement by its first keyword.
pub fn classify_kind(sql: &str) -> &'static str {
    let kw = sql.trim_start();
    let starts = |p: &str| kw.len() >= p.len() && kw[..p.len()].eq_ignore_ascii_case(p);
    if starts("select") {
        "select"
    } else if starts("insert") {
        "insert"
    } else if starts("update") {
        "update"
    } else if starts("delete") {
        "delete"
    } else if starts("pragma") {
        "pragma"
    } else if starts("create") {
        "ddl"
    } else if starts("checkpoint") {
        "checkpoint"
    } else {
        "other"
    }
}

/// Drop guard decrementing the open-connections gauge. Tie its lifetime to
/// the connection it tracks.
pub struct ConnectionGuard {
    database: &'static str,
    role: &'static str,
}

impl ConnectionGuard {
    fn acquire(role: &'static str) -> Self {
        metrics::gauge!("sqlite_connections_open", "database" => DATABASE, "role" => role)
            .increment(1.0);
        Self {
            database: DATABASE,
            role,
        }
    }

    /// Explicit release for long-lived connections not tied to a guard.
    pub fn release(role: &'static str) {
        metrics::gauge!("sqlite_connections_open", "database" => DATABASE, "role" => role)
            .decrement(1.0);
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        metrics::gauge!("sqlite_connections_open", "database" => self.database, "role" => self.role)
            .decrement(1.0);
    }
}

fn error_class(err: &sqlx::Error) -> &'static str {
    match err {
        sqlx::Error::Database(db) => match db.code().and_then(|c| c.parse::<i32>().ok()).unwrap_or(0) {
            5 => "busy",
            6 => "locked",
            19 | 2067 => "constraint",
            _ => "other",
        },
        _ => "other",
    }
}

pub fn record_error(kind: &'static str, err: &sqlx::Error) {
    metrics::counter!("sqlite_errors_total", "database" => DATABASE, "kind" => kind, "error" => error_class(err))
        .increment(1);
}

pub fn record_statement(
    kind: &'static str,
    rows_read: u64,
    rows_written: u64,
    elapsed: Duration,
) {
    metrics::counter!("sqlite_statements_total", "database" => DATABASE, "kind" => kind)
        .increment(1);
    if rows_read > 0 {
        metrics::counter!("sqlite_statement_rows_total", "database" => DATABASE, "kind" => kind, "direction" => "read")
            .increment(rows_read);
    }
    if rows_written > 0 {
        metrics::counter!("sqlite_statement_rows_total", "database" => DATABASE, "kind" => kind, "direction" => "written")
            .increment(rows_written);
    }
    metrics::histogram!("sqlite_statement_seconds", "database" => DATABASE, "kind" => kind)
        .record(elapsed.as_secs_f64());
}

/// A SELECT result set: count returned rows as reads.
pub fn record_select(rows: usize, elapsed: Duration) {
    record_statement("select", rows as u64, 0, elapsed);
}

/// Open a per-query read connection (sipflow opens one per hour bucket per
/// query). Returns the connection plus a guard that keeps
/// `sqlite_connections_open` accurate until the connection drops.
pub async fn connect_read(
    db_path: &Path,
) -> Result<(SqliteConnection, ConnectionGuard), sqlx::Error> {
    let start = Instant::now();
    let res = SqliteConnection::connect(&format!("sqlite:{}", db_path.display())).await;
    match &res {
        Ok(_) => {
            metrics::counter!("sqlite_connections_opened_total", "database" => DATABASE, "role" => "read")
                .increment(1);
            metrics::histogram!("sqlite_connection_open_seconds", "database" => DATABASE, "role" => "read")
                .record(start.elapsed().as_secs_f64());
        }
        Err(e) => {
            metrics::counter!("sqlite_connection_errors_total", "database" => DATABASE, "phase" => "connect")
                .increment(1);
            record_error("connect", e);
        }
    }
    res.map(|c| (c, ConnectionGuard::acquire("read")))
}

/// Open metrics for the long-lived per-bucket write connection.
pub fn record_write_open(elapsed: Duration) {
    metrics::counter!("sqlite_connections_opened_total", "database" => DATABASE, "role" => "write")
        .increment(1);
    metrics::histogram!("sqlite_connection_open_seconds", "database" => DATABASE, "role" => "write")
        .record(elapsed.as_secs_f64());
    ConnectionGuard::acquire("write");
}

/// Begin-to-end timer for a write transaction.
pub struct TxTimer {
    start: Instant,
}

impl TxTimer {
    pub fn begin() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn finish(self, committed: bool) {
        metrics::counter!("sqlite_transactions_total", "database" => DATABASE, "outcome" => if committed { "commit" } else { "rollback" })
            .increment(1);
        metrics::histogram!("sqlite_transaction_seconds", "database" => DATABASE)
            .record(self.start.elapsed().as_secs_f64());
    }
}

pub fn record_checkpoint(kind: &'static str, elapsed: Duration, busy: u64) {
    metrics::counter!("sqlite_wal_checkpoint_total", "database" => DATABASE, "kind" => kind)
        .increment(1);
    metrics::histogram!("sqlite_wal_checkpoint_seconds", "database" => DATABASE, "kind" => kind)
        .record(elapsed.as_secs_f64());
    if busy > 0 {
        metrics::counter!("sqlite_wal_checkpoint_busy_total", "database" => DATABASE)
            .increment(busy);
    }
}

pub fn set_gauge(name_file: &str, bytes: u64) {
    match name_file {
        "db" => metrics::gauge!("sqlite_db_bytes", "database" => DATABASE).set(bytes as f64),
        "wal" => metrics::gauge!("sqlite_wal_bytes", "database" => DATABASE).set(bytes as f64),
        _ => {}
    }
}

pub fn set_page_gauges(page_count: u64, freelist_pages: u64) {
    metrics::gauge!("sqlite_page_count", "database" => DATABASE).set(page_count as f64);
    metrics::gauge!("sqlite_freelist_pages", "database" => DATABASE).set(freelist_pages as f64);
}

#[cfg(test)]
mod tests {
    use super::classify_kind;

    #[test]
    fn classify_first_keyword() {
        assert_eq!(classify_kind("SELECT * FROM t"), "select");
        assert_eq!(classify_kind("  insert INTO t VALUES (1)"), "insert");
        assert_eq!(classify_kind("PRAGMA wal_checkpoint(PASSIVE)"), "pragma");
        assert_eq!(classify_kind("CREATE INDEX i ON t(c)"), "ddl");
        assert_eq!(classify_kind("explain select 1"), "other");
    }
}
