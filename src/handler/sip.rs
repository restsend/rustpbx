use super::call::CallParams;
use super::StreamOption;
use crate::app::AppState;
use crate::handler::call::{handle_call, ActiveCallType, CallHandlerState};
use crate::media::track::rtp::{RtpTrack, RtpTrackBuilder};
use crate::media::track::TrackConfig;
use crate::TrackId;
use anyhow::Result;
use axum::extract::{Query, WebSocketUpgrade};
use axum::{extract::State, response::Response};
use rsipstack::dialog::invitation::InviteOption;
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
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

pub async fn sip_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CallParams>,
) -> Response {
    let session_id = params.id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let state_clone = state.clone();
    ws.on_upgrade(|socket| async move {
        info!("sip call: {session_id}");
        let start_time = Instant::now();
        match handle_call(ActiveCallType::Sip, session_id.clone(), socket, state).await {
            Ok(_) => (),
            Err(e) => {
                error!("Error handling SIP connection: {}", e);
            }
        }
        let mut active_calls = state_clone.active_calls.lock().await;
        active_calls.remove(&session_id);
        info!(
            "sip call: hangup, duration {}s",
            start_time.elapsed().as_secs_f32()
        );
    })
}

pub(super) async fn new_rtp_track_with_sip(
    state: AppState,
    token: CancellationToken,
    track_id: TrackId,
    track_config: TrackConfig,
    option: &StreamOption,
) -> Result<RtpTrack> {
    let ua = state.useragent.clone();
    let caller = match option.caller {
        Some(ref caller) => caller.clone(),
        None => return Err(anyhow::anyhow!("caller is required")),
    };
    let callee = match option.callee {
        Some(ref callee) => callee.clone(),
        None => return Err(anyhow::anyhow!("callee is required")),
    };
    let mut rtp_track =
        RtpTrackBuilder::new(track_id, track_config).with_cancel_token(token.child_token());

    if let Some(rtp_start_port) = state.config.rtp_start_port {
        rtp_track = rtp_track.with_rtp_start_port(rtp_start_port);
    }

    if let Some(ref external_ip) = state.config.external_ip {
        rtp_track = rtp_track.with_external_addr(external_ip.parse()?);
    }

    if let Some(ref stun_server) = state.config.stun_server {
        rtp_track = rtp_track.with_stun_server(stun_server.clone());
    }

    let rtp_track = rtp_track.build().await?;

    let invite_option = InviteOption {
        caller: caller.clone().try_into()?,
        callee: callee.try_into()?,
        content_type: Some("application/sdp".to_string()),
        offer: rtp_track
            .local_description()
            .ok()
            .map(|s| s.as_bytes().to_vec()),
        contact: caller.try_into()?,
        credential: None,
    };

    match ua.invite(invite_option).await {
        Ok((_dialog_id, answer)) => {
            match answer {
                Some(answer) => {
                    let answer = String::from_utf8(answer)?;
                    rtp_track.set_remote_description(answer.as_str());
                }
                None => return Err(anyhow::anyhow!("failed to get answer")),
            }
            Ok(rtp_track)
        }
        Err(e) => Err(anyhow::anyhow!("failed to invite: {}", e)),
    }
}
