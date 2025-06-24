use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum ProxyStatus {}

pub type ProxyStatusSender = broadcast::Sender<ProxyStatus>;
pub type ProxyStatusReceiver = broadcast::Receiver<ProxyStatus>;
