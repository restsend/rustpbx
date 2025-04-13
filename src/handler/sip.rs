use crate::handler::call::CallHandlerState;
use axum::extract::{Query, WebSocketUpgrade};
use axum::{extract::State, response::Response};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tracing::info;
use uuid::Uuid;

use super::call::CallParams;

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

pub async fn sip_handler(
    ws: WebSocketUpgrade,
    State(state): State<CallHandlerState>,
    Query(params): Query<CallParams>,
) -> Response {
    let session_id = params.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let state_clone = state.clone();
    ws.on_upgrade(|_socket| async move {
        info!("sip call: {session_id}");
        let start_time = Instant::now();

        let mut active_calls = state_clone.active_calls.lock().await;
        active_calls.remove(&session_id);
        info!(
            "sip call: hangup, duration {}s",
            start_time.elapsed().as_secs_f32()
        );
    })
}

// pub async fn sip_handler(
//     State(state): State<CallHandlerState>,
//     Extension(useragent): Extension<Arc<UserAgent>>,
//     Json(req): Json<SipCallRequest>,
// ) -> Response {
//     info!("Initiating SIP call to {}", req.callee);

//     // Generate a unique call ID
//     let call_id = Uuid::new_v4().to_string();

//     // Create credential if username/password are provided
//     let credential = match (req.username.as_ref(), req.password.as_ref()) {
//         (Some(username), Some(password)) => Some(Credential {
//             username: username.clone(),
//             password: password.clone(),
//         }),
//         _ => None,
//     };

//     // Create invitation options
//     let invite_options = InviteOptions {
//         callee: req.callee.clone(),
//         credential,
//     };

//     // Create a cancellation token for this call
//     let token = CancellationToken::new();

//     // Clone what we need for the async task
//     let ua = useragent.clone();
//     let callee = req.callee.clone();
//     let call_id_clone = call_id.clone();
//     let state_clone = state.clone();
//     let _media_type = req.media_type.clone();

//     // Spawn a task to handle the call
//     tokio::spawn(async move {
//         match ua.invite(callee.clone(), invite_options).await {
//             Ok(_) => {
//                 info!("SIP call initiated successfully to {}", callee);

//                 // Track ID for this call
//                 let _track_id = format!("sip-call-{}", &call_id_clone);

//                 // Create event sender for this media stream
//                 let event_sender = create_event_sender();

//                 // Create a media stream for this call
//                 let media_stream = MediaStreamBuilder::new(event_sender)
//                     .with_id(call_id_clone.clone())
//                     .cancel_token(token.clone())
//                     .build();

//                 // Add the call to the active calls
//                 let mut active_calls = state_clone.active_calls.lock().await;
//                 let session_id = call_id_clone.clone();
//                 active_calls.insert(
//                     call_id_clone.clone(),
//                     Arc::new(ActiveCall {
//                         cancel_token: token.clone(),
//                         call_type: ActiveCallType::Sip,
//                         session_id,
//                         options: crate::handler::StreamOptions::default(),
//                         created_at: chrono::Utc::now(),
//                         media_stream: Arc::new(media_stream),
//                         track_config: TrackConfig::default(),
//                         tts_command_tx: tokio::sync::Mutex::new(None),
//                         tts_config: None,
//                     }),
//                 );

//                 info!("Added SIP call to active calls with ID {}", call_id_clone);
//             }
//             Err(e) => {
//                 debug!("Failed to initiate SIP call to {}: {:?}", callee, e);
//             }
//         }
//     });

//     // Return success response immediately
//     Json(SipCallResponse {
//         call_id,
//         status: "initiated".to_string(),
//     })
//     .into_response()
// }
