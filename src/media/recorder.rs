pub struct Recorder {
    pub path: String,
}

impl Recorder {
    pub fn new(path: String) -> Self {
        Self { path }
    }
}
