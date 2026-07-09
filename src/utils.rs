use sea_orm::sea_query::{Func, IntoCondition, SimpleExpr};
use std::sync::atomic::{AtomicI64, Ordering};

pub fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            '~' | ',' | '|' | '.' | '/' | '[' | '{' | '}' | ']' | '=' | '&' | '%' | '$' | '\\'
            | '"' | '\'' | '`' | '<' | '>' | '?' | ':' | ';' | '*' | '+' | '#' => '_',
            _ => c,
        })
        .collect()
}

/// Database query helper: `COUNT(CASE WHEN condition THEN 1 END)`.
pub fn count_when<C>(condition: C) -> SimpleExpr
where
    C: IntoCondition,
{
    Func::count(sea_orm::sea_query::Expr::case(
        condition,
        sea_orm::sea_query::Expr::val(1),
    ))
    .into()
}

/// Global active task counter (atomic, no lock contention).
pub static GLOBAL_TASK_COUNT: AtomicI64 = AtomicI64::new(0);

pub struct TaskGuard;

impl TaskGuard {
    pub fn new(_loc: String) -> Self {
        GLOBAL_TASK_COUNT.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        GLOBAL_TASK_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
}

#[track_caller]
pub fn spawn<T>(future: T) -> tokio::task::JoinHandle<T::Output>
where
    T: std::future::Future + Send + 'static,
    T::Output: Send + 'static,
{
    let location = std::panic::Location::caller();
    let loc = format!("{}:{}", location.file(), location.line());
    let _guard = TaskGuard::new(loc);
    tokio::spawn(async move {
        let _guard = _guard;
        future.await
    })
}

/// Get current active task count
pub fn active_task_count() -> usize {
    GLOBAL_TASK_COUNT.load(Ordering::Relaxed) as usize
}

/// Get task count by location prefix (stub — always returns 0, kept for test compat)
pub fn active_task_count_by_prefix(_prefix: &str) -> usize {
    0
}

/// Get detailed task metrics (stub — returns empty map, kept for compat)
pub fn task_metrics_snapshot() -> std::collections::HashMap<String, usize> {
    std::collections::HashMap::new()
}

/// Reset all metrics (useful for tests)
pub fn reset_task_metrics() {
    GLOBAL_TASK_COUNT.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("session~123"), "session_123");
        assert_eq!(sanitize_id("leg|456,"), "leg_456_");
        assert_eq!(sanitize_id("path/to/id"), "path_to_id");
        assert_eq!(sanitize_id("id.with.dots"), "id_with_dots");
        assert_eq!(sanitize_id("brackets[{}]"), "brackets____");
        assert_eq!(sanitize_id("symbols=&%$"), "symbols____");
        assert_eq!(sanitize_id("safe-id_123"), "safe-id_123");
        assert_eq!(sanitize_id("more:;*+#"), "more_____");
    }
}
