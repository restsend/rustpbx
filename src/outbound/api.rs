//! Axum handler for `POST /ami/v1/outbound/dial` — SSE endpoint.
//!
//! The SSE stream is a **pure RWI event passthrough**: every event that the
//! gateway emits for this `call_id` is forwarded verbatim as an SSE event.
//! Zero custom event types. The stream closes (EOF) when:
//!   - a failure event arrives before answer (`call_busy` / `call_no_answer` /
//!     `call_hangup`), or
//!   - `call_answered` arrives and the post-answer dispatcher completes.

use crate::outbound::OutboundContext;
use crate::outbound::dispatcher::{DispatchOutcome, dispatch};
use crate::outbound::events::{SseEntry, encode_rwi_event, is_call_failure_event};
use crate::outbound::request::DialRequest;
use crate::rwi::processor::RwiCommandProcessor;
use crate::rwi::session::{OriginateRequest, RwiCommandPayload};
use axum::Json;
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use tokio::sync::mpsc;

/// Build the outbound sub-router. The caller is responsible for applying
/// auth middleware (AMI IP allowlist).
pub fn router() -> axum::Router<OutboundContext> {
    axum::Router::new().route("/dial", axum::routing::post(dial))
}

/// POST /outbound/dial — HTTP entry point.
pub async fn dial(
    axum::extract::State(ctx): axum::extract::State<OutboundContext>,
    Json(req): Json<DialRequest>,
) -> Response {
    execute_dial_response(ctx, req).await
}

/// Core entry point — builds the SSE stream from a context + request.
pub async fn execute_dial_response(ctx: OutboundContext, req: DialRequest) -> Response {
    match execute_dial_core(ctx, req).await {
        Ok(rx) => {
            let stream = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|entry| {
                    let sse = SseEvent::default()
                        .event(&entry.event)
                        .data(&entry.data);
                    (Ok::<_, std::convert::Infallible>(sse), rx)
                })
            });
            Sse::new(stream)
                .keep_alive(
                    KeepAlive::new()
                        .interval(std::time::Duration::from_secs(15))
                        .text("keep-alive"),
                )
                .into_response()
        }
        Err(resp) => resp.into_response(),
    }
}

/// Core logic — originates the call and returns a receiver of `SseEntry`s.
///
/// Each `SseEntry` carries an RWI event type name and the JSON-serialized
/// payload. The SSE wrapper converts these to axum `Event`s. Tests can
/// inspect `SseEntry` fields directly.
pub async fn execute_dial_core(
    ctx: OutboundContext,
    req: DialRequest,
) -> Result<mpsc::UnboundedReceiver<SseEntry>, (StatusCode, Json<serde_json::Value>)> {
    let call_id = req
        .call_id
        .clone()
        .unwrap_or_else(|| format!("outbound-{}", uuid::Uuid::new_v4()));

    let ring_timeout = req
        .ring_timeout
        .unwrap_or(ctx.config.default_ring_timeout);
    let webhook_timeout = std::time::Duration::from_secs(ctx.config.default_webhook_timeout);

    // Subscribe BEFORE originating so we don't miss early events.
    let mut event_rx = ctx.gateway.read().subscribe_events();

    // Build the processor.
    let processor = RwiCommandProcessor::new(
        ctx.call_registry.clone(),
        ctx.gateway.clone(),
        ctx.conference_manager.clone(),
    )
    .with_sip_server(ctx.sip_server.clone());

    let destination = normalize_destination(&req.destination, &ctx);
    let caller_id = req.caller_id.clone().or_else(|| {
        let realm = ctx
            .sip_server
            .proxy_config
            .realms
            .as_ref()
            .and_then(|v| v.first().cloned())
            .unwrap_or_else(|| ctx.sip_server.proxy_config.addr.clone());
        Some(format!("sip:outbound@{}", realm))
    });

    let originate_req = OriginateRequest {
        call_id: call_id.clone(),
        destination,
        caller_id,
        timeout_secs: Some(ring_timeout as u32),
        hold_music: None,
        hold_music_target: None,
        ringback: None,
        ringback_target: None,
        extra_headers: req.extra_headers.clone(),
        trunk: req.trunk.clone(),
    };

    // Fire the originate — events arrive via the gateway tap.
    if let Err(e) = processor
        .process_command(RwiCommandPayload::Originate(originate_req))
        .await
    {
        tracing::warn!(%call_id, error = %e, "outbound originate rejected");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "call_id": call_id,
                "error": format!("{:?}", e),
            })),
        ));
    }

    let (tx, rx) = mpsc::unbounded_channel::<SseEntry>();

    let on_answer = req.on_answer.clone();
    let metadata = req.metadata.clone();
    let caller_for_dispatch = req.caller_id.clone().unwrap_or_default();
    let callee_for_dispatch = req.destination.clone();
    let answer_timeout = std::time::Duration::from_secs(ctx.config.default_answer_timeout);
    let http_client = ctx.http_client.clone();

    // Fresh processor for the dispatch task (RwiCommandProcessor is not Clone).
    let dispatch_processor = RwiCommandProcessor::new(
        ctx.call_registry.clone(),
        ctx.gateway.clone(),
        ctx.conference_manager.clone(),
    )
    .with_sip_server(ctx.sip_server.clone());

    crate::utils::spawn(async move {
        let deadline = tokio::time::Instant::now() + answer_timeout;
        let answered = false;

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    // Safety-net timeout — in normal operation the originate's
                    // own ring_timeout fires first and emits call_no_answer.
                    break;
                }
                recv = event_rx.recv() => {
                    match recv {
                        Ok(entry) => {
                            if entry.call_id != call_id {
                                continue;
                            }

                            let et = entry.event.event_type;

                            // Pass through the RWI event verbatim.
                            let _ = tx.send(encode_rwi_event(&entry));

                            if et == "call_answered" && !answered {
                                // Run the post-answer dispatcher (inline).
                                let outcome: DispatchOutcome = dispatch(
                                    &dispatch_processor,
                                    &http_client,
                                    &call_id,
                                    &callee_for_dispatch,
                                    &caller_for_dispatch,
                                    &metadata,
                                    &on_answer,
                                    webhook_timeout,
                                ).await;

                                if !outcome.success {
                                    tracing::warn!(
                                        %call_id,
                                        detail = %outcome.detail,
                                        "post-answer dispatch failed"
                                    );
                                }

                                // Non-blocking drain: collect any RWI events
                                // produced by the dispatcher (call_bridged,
                                // queue_joined, etc.) that are already in the
                                // broadcast channel buffer.
                                while let Ok(entry) = event_rx.try_recv() {
                                    if entry.call_id == call_id {
                                        let _ = tx.send(encode_rwi_event(&entry));
                                    }
                                }

                                // Close the SSE stream.
                                break;
                            }

                            if !answered && is_call_failure_event(et) {
                                // Originate failed — the failure event has
                                // already been passed through above.
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Silent skip — tolerate lag without notifying.
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok(rx)
}

/// Convert a bare phone number into a SIP URI using the server's realm.
fn normalize_destination(dest: &str, ctx: &OutboundContext) -> String {
    if dest.starts_with("sip:") {
        return dest.to_string();
    }
    let realm = ctx
        .sip_server
        .proxy_config
        .realms
        .as_ref()
        .and_then(|v| v.first().cloned())
        .unwrap_or_else(|| ctx.sip_server.proxy_config.addr.clone());
    format!("sip:{}@{}", dest, realm)
}
