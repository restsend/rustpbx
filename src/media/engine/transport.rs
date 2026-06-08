//! Transport helpers for MediaEngine legs.

/// Resolve an audio file path, handling relative paths and HTTP URLs.
///
/// Currently only HTTP/HTTPS URLs and absolute paths are meaningful; all other
/// inputs are returned as-is and validated at open-time by the caller.
pub fn resolve_audio_path(audio_file: &str) -> String {
    audio_file.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_audio_path_absolute() {
        let p = resolve_audio_path("/tmp/test.wav");
        assert_eq!(p, "/tmp/test.wav");
    }

    #[test]
    fn test_resolve_audio_path_http() {
        let p = resolve_audio_path("https://example.com/test.wav");
        assert_eq!(p, "https://example.com/test.wav");
    }
}
