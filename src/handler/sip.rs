use super::CallOption;
use crate::app::AppState;
use crate::media::track::rtp::{RtpTrack, RtpTrackBuilder};
use crate::media::track::TrackConfig;
use crate::useragent::invitation::PendingDialog;
use crate::TrackId;
use anyhow::Result;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog::DialogStateSender;
use rsipstack::dialog::invitation::InviteOption;
use rsipstack::dialog::DialogId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

#[derive(Debug, Deserialize, Serialize, Default, Clone)]
#[serde(default)]
pub struct SipOption {
    pub username: String,
    pub password: String,
    pub realm: String,
    pub headers: Option<HashMap<String, String>>,
}

pub async fn new_rtp_track_with_pending_call(
    state: AppState,
    token: CancellationToken,
    track_id: TrackId,
    track_config: TrackConfig,
    _option: &CallOption,
    dlg_state_sender: DialogStateSender,
    pending_call: PendingDialog,
) -> Result<(DialogId, RtpTrack)> {
    let mut rtp_track =
        RtpTrackBuilder::new(track_id.clone(), track_config).with_cancel_token(token);

    if let Some(ref sip) = state.config.ua {
        if let Some(rtp_start_port) = sip.rtp_start_port {
            rtp_track = rtp_track.with_rtp_start_port(rtp_start_port);
        }
        if let Some(rtp_end_port) = sip.rtp_end_port {
            rtp_track = rtp_track.with_rtp_end_port(rtp_end_port);
        }

        if let Some(ref external_ip) = sip.external_ip {
            rtp_track = rtp_track.with_external_addr(external_ip.parse()?);
        }

        if let Some(ref stun_server) = sip.stun_server {
            rtp_track = rtp_track.with_stun_server(stun_server.clone());
        }
    }

    let mut rtp_track = rtp_track.build().await?;
    let initial_request = pending_call.dialog.initial_request();
    let offer = String::from_utf8_lossy(&initial_request.body);
    match rtp_track.set_remote_description(&offer) {
        Ok(_) => (),
        Err(e) => {
            error!("failed to set remote description: {}", e);
            return Err(anyhow::anyhow!("failed to set remote description"));
        }
    }

    let answer = match rtp_track.local_description() {
        Ok(answer) => answer,
        Err(e) => {
            error!("failed to get local description: {}", e);
            return Err(anyhow::anyhow!("failed to get local description"));
        }
    };
    let headers = vec![rsip::Header::ContentType(
        "application/sdp".to_string().into(),
    )];

    match pending_call
        .dialog
        .accept(Some(headers), Some(answer.as_bytes().to_vec()))
    {
        Ok(_) => (),
        Err(e) => {
            error!("failed to accept call: {}", e);
            return Err(anyhow::anyhow!("failed to accept call"));
        }
    }

    let dialog_id = pending_call.dialog.id();

    let mut state_receiver = pending_call.state_receiver;
    let token_clone = pending_call.token;
    tokio::spawn(async move {
        tokio::select! {
            _ = token_clone.cancelled() => {}
            _ = async {
                while let Some(state) = state_receiver.recv().await {
                    if let Err(_) = dlg_state_sender.send(state) {
                        break;
                    }
                }
            } => {}
        }
    });

    Ok((dialog_id, rtp_track))
}

pub async fn new_rtp_track_with_sip(
    state: AppState,
    token: CancellationToken,
    track_id: TrackId,
    track_config: TrackConfig,
    option: &CallOption,
    dlg_state_sender: DialogStateSender,
) -> Result<(DialogId, RtpTrack)> {
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
        RtpTrackBuilder::new(track_id.clone(), track_config).with_cancel_token(token);

    if let Some(ref sip) = state.config.ua {
        if let Some(rtp_start_port) = sip.rtp_start_port {
            rtp_track = rtp_track.with_rtp_start_port(rtp_start_port);
        }
        if let Some(rtp_end_port) = sip.rtp_end_port {
            rtp_track = rtp_track.with_rtp_end_port(rtp_end_port);
        }

        if let Some(ref external_ip) = sip.external_ip {
            rtp_track = rtp_track.with_external_addr(external_ip.parse()?);
        }

        if let Some(ref stun_server) = sip.stun_server {
            rtp_track = rtp_track.with_stun_server(stun_server.clone());
        }
    }

    let mut rtp_track = rtp_track.build().await?;
    let offer = rtp_track.local_description().ok();

    let headers = if let Some(ref headers) = option.sip.as_ref().unwrap().headers {
        Some(
            headers
                .iter()
                .map(|(k, v)| rsip::Header::Other(k.clone(), v.clone()))
                .collect(),
        )
    } else {
        None
    };
    let invite_option = InviteOption {
        caller: caller.clone().try_into()?,
        callee: callee.try_into()?,
        content_type: Some("application/sdp".to_string()),
        offer: offer.as_ref().map(|s| s.as_bytes().to_vec()),
        contact: caller.try_into()?,
        credential: option.sip.as_ref().map(|opt| Credential {
            username: opt.username.clone(),
            password: opt.password.clone(),
            realm: Some(opt.realm.clone()),
        }),
        headers,
    };
    info!(
        "invite {} -> {} offer: \n{:?}",
        invite_option.caller, invite_option.callee, offer
    );
    match ua.invite(invite_option, dlg_state_sender).await {
        Ok((dialog_id, answer)) => {
            match answer {
                Some(answer) => {
                    let answer = String::from_utf8_lossy(&answer);
                    match rtp_track.set_remote_description(&answer) {
                        Ok(_) => (),
                        Err(e) => {
                            error!("sip_call:failed to set remote description: {}", e);
                            return Err(anyhow::anyhow!("failed to set remote description"));
                        }
                    }
                }
                None => return Err(anyhow::anyhow!("failed to get answer")),
            }
            Ok((dialog_id, rtp_track))
        }
        Err(e) => Err(e),
    }
}
