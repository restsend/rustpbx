use tokio_util::sync::CancellationToken;

pub struct Recorder {
    pub cancel_token: CancellationToken,
    pub file_name: String,
}

impl Recorder {
    pub async fn process(&self) {
        todo!()
    }
}
