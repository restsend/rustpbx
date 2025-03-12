pub struct VADConfig {
    threshold: f32,
}

impl Default for VADConfig {
    fn default() -> Self {
        Self { threshold: 0.85 }
    }
}
