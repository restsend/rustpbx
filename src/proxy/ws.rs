use crate::handler::middleware::clientaddr::ClientAddr;
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use rsip::SipMessage;
use rsipstack::{
    transaction::endpoint::EndpointInnerRef,
    transport::{channel::ChannelConnection, SipAddr, SipConnection, TransportEvent},
};
use tokio::{select, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub async fn sip_ws_handler(
    token: CancellationToken,
    client_addr: ClientAddr,
    socket: WebSocket,
    endpoint_ref: EndpointInnerRef,
) {
    let (mut ws_sink, mut ws_read) = socket.split();
    let (from_ws_tx, from_ws_rx) = mpsc::unbounded_channel();
    let (to_ws_tx, mut to_ws_rx) = mpsc::unbounded_channel();

    let transport_type = rsip::transport::Transport::Ws;
    let local_addr = SipAddr {
        r#type: Some(transport_type),
        addr: client_addr.addr.clone().into(),
    };

    let connection = match ChannelConnection::create_connection(
        from_ws_rx,
        to_ws_tx,
        local_addr.clone(),
    )
    .await
    {
        Ok(conn) => conn,
        Err(e) => {
            error!("Failed to create channel connection: {:?}", e);
            return;
        }
    };

    let sip_connection = SipConnection::Channel(connection.clone());
    info!(
        addr = ?local_addr,
        "Created WebSocket channel connection"
    );

    endpoint_ref
        .transport_layer
        .add_connection(sip_connection.clone());

    // Use select! instead of spawning multiple tasks
    let local_addr_clone = local_addr.clone();
    let sip_connection_clone = sip_connection.clone();
    let read_from_websocket_look = async move {
        while let Some(Ok(message)) = ws_read.next().await {
            match message {
                Message::Text(text) => match SipMessage::try_from(text.as_str()) {
                    Ok(sip_msg) => {
                        debug!(
                            addr = ?local_addr_clone,
                            "WebSocket received SIP message: {}",
                            sip_msg.to_string().lines().next().unwrap_or("")
                        );
                        let msg = match SipConnection::update_msg_received(
                            sip_msg,
                            client_addr.addr,
                            transport_type,
                        ) {
                            Ok(msg) => msg,
                            Err(e) => {
                                error!("Error updating SIP via: {:?}", e);
                                continue;
                            }
                        };
                        if let Err(e) = from_ws_tx.send(TransportEvent::Incoming(
                            msg,
                            sip_connection_clone.clone(),
                            local_addr_clone.clone(),
                        )) {
                            error!("Error forwarding message to transport: {:?}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Error parsing SIP message from WebSocket: {}", e);
                    }
                },
                Message::Binary(bin) => match SipMessage::try_from(bin) {
                    Ok(sip_msg) => {
                        debug!(
                            addr = ?local_addr_clone,
                            "WebSocket received binary SIP message: {}",
                            sip_msg.to_string().lines().next().unwrap_or("")
                        );
                        if let Err(e) = from_ws_tx.send(TransportEvent::Incoming(
                            sip_msg,
                            sip_connection_clone.clone(),
                            local_addr_clone.clone(),
                        )) {
                            error!("Error forwarding binary message to transport: {:?}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("Error parsing binary SIP message from WebSocket: {}", e);
                    }
                },
                Message::Close(_) => {
                    info!("WebSocket connection closed by client");
                    break;
                }
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    };
    let local_addr_clone = local_addr.clone();
    let write_to_websocket_look = async move {
        while let Some(event) = to_ws_rx.recv().await {
            match event {
                TransportEvent::Incoming(sip_msg, _, _) => {
                    let message_text = sip_msg.to_string();
                    debug!(
                        addr = ?local_addr_clone,
                        "Forwarding message to WebSocket: {}",
                        message_text.lines().next().unwrap_or("")
                    );
                    if let Err(e) = ws_sink.send(Message::Text(message_text.into())).await {
                        error!("Error sending message to WebSocket: {}", e);
                        break;
                    }
                }
                TransportEvent::New(_) => {}
                TransportEvent::Closed(_) => {
                    info!("Transport connection closed");
                    break;
                }
            }
        }
    };

    select! {
        _ = token.cancelled() => {
            info!("WebSocket connection cancelled");
        }
        _ = read_from_websocket_look => {
            info!(
                addr = ?local_addr,
                "WebSocket connection read task finished");
        }
        _ = write_to_websocket_look => {
            info!(
                addr = ?local_addr,
                "WebSocket connection write task finished");
        }
    }
    endpoint_ref.transport_layer.del_connection(&local_addr);
    info!("WebSocket connection handler exiting");
}
