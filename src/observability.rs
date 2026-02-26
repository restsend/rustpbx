use std::sync::OnceLock;
use tracing_subscriber::{Layer, Registry, reload};

pub type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

static RELOAD_HANDLE: OnceLock<reload::Handle<Option<BoxedLayer>, Registry>> = OnceLock::new();

pub fn init_reload_layer() -> impl Layer<Registry> + Send + Sync + 'static {
    let (layer, handle) = reload::Layer::new(None::<BoxedLayer>);
    let _ = RELOAD_HANDLE.set(handle);
    layer
}

pub fn install_otel_layer(layer: impl Layer<Registry> + Send + Sync + 'static) {
    if let Some(handle) = RELOAD_HANDLE.get() {
        if let Err(e) = handle.modify(|slot| *slot = Some(Box::new(layer))) {
            tracing::warn!("failed to install OTel tracing layer: {}", e);
        } else {
            tracing::info!("OpenTelemetry tracing layer installed");
        }
    } else {
        tracing::warn!(
            "install_otel_layer called before init_reload_layer; \
             OTel traces will not flow to the subscriber"
        );
    }
}

/// Remove the OTel layer (e.g. on graceful shutdown so in-flight spans are flushed).
pub fn remove_otel_layer() {
    if let Some(handle) = RELOAD_HANDLE.get() {
        let _ = handle.modify(|slot| *slot = None);
    }
}
