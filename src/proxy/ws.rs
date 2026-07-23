use crate::handler::middleware::clientaddr::ClientAddr;
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use rsipstack::sip::{SipMessage, prelude::HeadersExt};
use rsipstack::{
    transaction::endpoint::EndpointInnerRef,
    transport::{SipAddr, SipConnection, TransportEvent, channel::ChannelConnection},
};
use std::time::Duration;
use tokio::{select, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub async fn sip_ws_handler(
    token: CancellationToken,
    client_addr: ClientAddr,
    socket: WebSocket,
    endpoint_ref: EndpointInnerRef,
) {
    let (mut ws_sink, mut ws_read) = socket.split();
    let (from_ws_tx, from_ws_rx) = mpsc::unbounded_channel();
    let (to_ws_tx, mut to_ws_rx) = mpsc::unbounded_channel();

    let transport_type = if client_addr.is_secure {
        rsipstack::sip::transport::Transport::Wss
    } else {
        rsipstack::sip::transport::Transport::Ws
    };
    let local_addr = SipAddr {
        r#type: Some(transport_type),
        addr: client_addr.addr.into(),
    };
    let ws_token = token.child_token();
    let connection = match ChannelConnection::create_connection(
        from_ws_rx,
        to_ws_tx,
        local_addr.clone(),
        Some(ws_token.clone()),
    )
    .await
    {
        Ok(conn) => conn,
        Err(e) => {
            warn!(addr = %local_addr, "failed to create channel connection: {}", e);
            return;
        }
    };

    let sip_connection = SipConnection::Channel(connection.clone());
    info!(
        addr = %local_addr,
        "created WebSocket channel connection"
    );

    endpoint_ref
        .transport_layer
        .add_connection(sip_connection.clone());

    const WS_PING_INTERVAL: Duration = Duration::from_secs(25);
    let local_addr_clone = local_addr.clone();
    let sip_connection_clone = sip_connection.clone();
    let read_from_websocket_loop = async move {
        loop {
            match ws_read.next().await {
                None => {
                    debug!(addr = %local_addr_clone, "WebSocket stream ended");
                    break;
                }
                Some(Err(e)) => {
                    debug!(addr = %local_addr_clone, "WebSocket read error: {}", e);
                    break;
                }
                Some(Ok(message)) => {
                    match message {
                        Message::Text(text) => {
                            let text = text.to_string();
                            match SipMessage::try_from(text.as_str()) {
                                Ok(sip_msg) => {
                                    debug!(
                                        addr = %local_addr_clone,
                                        cseq = sip_msg.cseq_header().ok().map(|c| c.value()).unwrap_or_default(),
                                        raw_message = text,
                                        "WebSocket received"
                                    );
                                    let msg = match SipConnection::update_msg_received(
                                        sip_msg,
                                        client_addr.addr,
                                        transport_type,
                                    ) {
                                        Ok(msg) => msg,
                                        Err(e) => {
                                            warn!(addr = %local_addr_clone, "error updating SIP via: {}", e);
                                            continue;
                                        }
                                    };
                                    if let Err(e) = from_ws_tx.send(TransportEvent::Incoming(
                                        msg,
                                        sip_connection_clone.clone(),
                                        local_addr_clone.clone(),
                                    )) {
                                        warn!(addr = %local_addr_clone, "error forwarding message to transport: {}", e);
                                        break;
                                    }
                                }
                                Err(e) => {
                                    warn!(addr = %local_addr_clone, "error parsing SIP message from WebSocket: {}", e);
                                }
                            }
                        }
                        Message::Binary(bin) => match SipMessage::try_from(bin) {
                            Ok(sip_msg) => {
                                debug!(
                                    addr = %local_addr_clone,
                                    "WebSocket received binary SIP message: \n{}",
                                    sip_msg.to_string()
                                );
                                if let Err(e) = from_ws_tx.send(TransportEvent::Incoming(
                                    sip_msg,
                                    sip_connection_clone.clone(),
                                    local_addr_clone.clone(),
                                )) {
                                    warn!(addr = %local_addr_clone, "error forwarding binary message to transport: {:?}", e);
                                    break;
                                }
                            }
                            Err(e) => {
                                warn!(addr = %local_addr_clone, "error parsing binary SIP message from WebSocket: {}", e);
                            }
                        },
                        Message::Close(_) => {
                            debug!(addr = %local_addr_clone, "WebSocket connection closed by client");
                            break;
                        }
                        // Activity timestamp refreshed above.  We do not respond
                        // to Ping here because `ws_sink` is owned by the write
                        // loop; tokio-tungstenite's default auto-pong handles
                        // it at the protocol level.
                        Message::Ping(_) => {}
                        Message::Pong(_) => {} // activity already tracked
                    }
                }
            }
        }
    };
    let local_addr_clone = local_addr.clone();
    let write_to_websocket_loop = async move {
        let mut ping_ticker = tokio::time::interval(WS_PING_INTERVAL);
        ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                event = to_ws_rx.recv() => {
                    let Some(event) = event else { break; };
                    match event {
                        TransportEvent::Incoming(sip_msg, _, _) => {
                            let raw_message = sip_msg.to_string();
                            let cseq = sip_msg.cseq_header().ok();
                            debug!(
                                addr = %local_addr_clone,
                                cseq = cseq.map(|c| c.value()).unwrap_or_default(),
                                raw_message,
                                "ws forwarding ",
                            );
                            if let Err(e) = ws_sink.send(Message::Text(raw_message.into())).await {
                                warn!(
                                addr = %local_addr_clone, "error sending message to WebSocket: {}", e);
                                break;
                            }
                        }
                        TransportEvent::New(_) => {}
                        TransportEvent::Closed(_) => {
                            info!(addr = %local_addr_clone, "transport connection closed");
                            break;
                        }
                    }
                }
                _ = ping_ticker.tick() => {
                    if let Err(e) = ws_sink.send(Message::Ping(bytes::Bytes::new())).await {
                        warn!(addr = %local_addr_clone, "error sending WebSocket ping: {}", e);
                        break;
                    }
                    debug!(addr = %local_addr_clone, "WebSocket ping sent");
                }
            }
        }
    };

    select! {
        _ = token.cancelled() => {
            info!(addr = %local_addr, "WebSocket connection cancelled");
        }
        _ = read_from_websocket_loop => {
            info!(addr = %local_addr, "WebSocket read loop exited");
        }
        _ = write_to_websocket_loop => {
            info!(addr = %local_addr, "WebSocket write loop exited");
        }
    }
    ws_token.cancel();
    endpoint_ref.transport_layer.del_connection(&local_addr);
}
