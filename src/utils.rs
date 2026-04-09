use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            '~' | ',' | '|' | '.' | '/' | '[' | '{' | '}' | ']' | '=' | '&' | '%' | '$' | '\\'
            | '"' | '\'' | '`' | '<' | '>' | '?' | ':' | ';' | '*' | '+' | '#' => '_',
            _ => c,
        })
        .collect()
}

pub struct TaskGuard {
    pub loc: String,
}

pub static GLOBAL_TASK_METRICS: once_cell::sync::Lazy<Arc<Mutex<HashMap<String, usize>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));

impl TaskGuard {
    pub fn new(loc: String) -> Self {
        if let Ok(mut metrics) = GLOBAL_TASK_METRICS.lock() {
            *metrics.entry(loc.clone()).or_insert(0) += 1;
        }
        Self { loc }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if let Ok(mut metrics) = GLOBAL_TASK_METRICS.lock() {
            if let Some(count) = metrics.get_mut(&self.loc) {
                if *count > 0 {
                    *count -= 1;
                }
            }
        }
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
    GLOBAL_TASK_METRICS.lock().unwrap().values().sum()
}

/// Get task count by location prefix (e.g., "src/rwi" for all RWI tasks)
pub fn active_task_count_by_prefix(prefix: &str) -> usize {
    GLOBAL_TASK_METRICS
        .lock()
        .unwrap()
        .iter()
        .filter(|(loc, _)| loc.starts_with(prefix))
        .map(|(_, count)| *count)
        .sum()
}

/// Get detailed task metrics
pub fn task_metrics_snapshot() -> HashMap<String, usize> {
    GLOBAL_TASK_METRICS.lock().unwrap().clone()
}

/// Reset all metrics (useful for tests)
pub fn reset_task_metrics() {
    GLOBAL_TASK_METRICS.lock().unwrap().clear();
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
