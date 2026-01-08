pub fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            '~' | ',' | '|' | '.' | '/' | '[' | '{' | '}' | ']' | '=' | '&' | '%' | '$' | '\\'
            | '"' | '\'' | '`' | '<' | '>' | '?' | ':' | ';' | '*' | '+' | '#' => '_',
            _ => c,
        })
        .collect()
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
