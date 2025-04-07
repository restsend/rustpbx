use crate::event::create_event_sender;
use crate::handler::call::{ActiveCall, ActiveCallType, CallHandlerState};
use crate::media::stream::MediaStreamBuilder;
use crate::media::track::TrackConfig;
use crate::useragent::useragent::{InviteOptions, UserAgent};
use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use rsipstack::dialog::authenticate::Credential;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use uuid::Uuid;

#[derive(Debug, Deserialize, Serialize)]
pub struct SipCallRequest {
    pub callee: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_media_type")]
    pub media_type: MediaType,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    WebRtc,
    Rtp,
}

fn default_media_type() -> MediaType {
    MediaType::WebRtc
}

#[derive(Debug, Serialize)]
pub struct SipCallResponse {
    pub call_id: String,
    pub status: String,
}

pub fn router() -> Router<CallHandlerState> {
    Router::new()
        .route("/call/sip", post(make_sip_call))
        .route("/call/sip/:id", get(get_sip_call))
}

pub async fn make_sip_call(
    State(state): State<CallHandlerState>,
    Extension(useragent): Extension<Arc<UserAgent>>,
    Json(req): Json<SipCallRequest>,
) -> Response {
    info!("Initiating SIP call to {}", req.callee);

    // Generate a unique call ID
    let call_id = Uuid::new_v4().to_string();

    // Create credential if username/password are provided
    let credential = match (req.username.as_ref(), req.password.as_ref()) {
        (Some(username), Some(password)) => Some(Credential {
            username: username.clone(),
            password: password.clone(),
        }),
        _ => None,
    };

    // Create invitation options
    let invite_options = InviteOptions {
        callee: req.callee.clone(),
        credential,
    };

    // Create a cancellation token for this call
    let token = CancellationToken::new();

    // Clone what we need for the async task
    let ua = useragent.clone();
    let callee = req.callee.clone();
    let call_id_clone = call_id.clone();
    let state_clone = state.clone();
    let _media_type = req.media_type.clone();

    // Spawn a task to handle the call
    tokio::spawn(async move {
        match ua.invite(callee.clone(), invite_options).await {
            Ok(_) => {
                info!("SIP call initiated successfully to {}", callee);

                // Track ID for this call
                let _track_id = format!("sip-call-{}", &call_id_clone);

                // Create event sender for this media stream
                let event_sender = create_event_sender();

                // Create a media stream for this call
                let media_stream = MediaStreamBuilder::new(event_sender)
                    .with_id(call_id_clone.clone())
                    .cancel_token(token.clone())
                    .build();

                // Add the call to the active calls
                let mut active_calls = state_clone.active_calls.lock().await;
                let session_id = call_id_clone.clone();
                active_calls.insert(
                    call_id_clone.clone(),
                    Arc::new(ActiveCall {
                        cancel_token: token.clone(),
                        call_type: ActiveCallType::Sip,
                        session_id,
                        options: crate::handler::StreamOptions::default(),
                        created_at: chrono::Utc::now(),
                        media_stream: Arc::new(media_stream),
                        track_config: TrackConfig::default(),
                        tts_command_tx: tokio::sync::Mutex::new(None),
                        tts_config: None,
                    }),
                );

                info!("Added SIP call to active calls with ID {}", call_id_clone);
            }
            Err(e) => {
                debug!("Failed to initiate SIP call to {}: {:?}", callee, e);
            }
        }
    });

    // Return success response immediately
    Json(SipCallResponse {
        call_id,
        status: "initiated".to_string(),
    })
    .into_response()
}

async fn get_sip_call(State(state): State<CallHandlerState>, Path(id): Path<String>) -> Response {
    let active_calls = state.active_calls.lock().await;

    if let Some(call) = active_calls.get(&id) {
        if matches!(call.call_type, ActiveCallType::Sip) {
            let created_at = call.created_at.to_rfc3339();
            return Json(serde_json::json!({
                "call_id": call.session_id,
                "status": "active",
                "created_at": created_at,
                "options": call.options,
            }))
            .into_response();
        }
    }

    Json(serde_json::json!({
        "status": "not_found",
        "message": format!("SIP call with ID {} not found", id),
    }))
    .into_response()
}
