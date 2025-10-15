use crate::handler::middleware::clientaddr::ClientAddr;
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use rsip::{
    SipMessage,
    prelude::{HasHeaders, HeadersExt, ToTypedHeader, UntypedHeader},
};
use rsipstack::{
    transaction::endpoint::EndpointInnerRef,
    transport::{SipAddr, SipConnection, TransportEvent, channel::ChannelConnection},
};
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
        rsip::transport::Transport::Wss
    } else {
        rsip::transport::Transport::Ws
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

    // Use select! instead of spawning multiple tasks
    let local_addr_clone = local_addr.clone();
    let sip_connection_clone = sip_connection.clone();
    let read_from_websocket_loop = async move {
        while let Some(Ok(message)) = ws_read.next().await {
            match message {
                Message::Text(text) => {
                    let text = text.to_string();
                    match SipMessage::try_from(text.as_str()) {
                        Ok(sip_msg) => {
                            debug!(
                                addr = %local_addr_clone,
                                cseq = sip_msg.cseq_header().ok().map(|c| c.value()).unwrap_or_default(),
                                "WebSocket received: \n{}",
                                text
                            );
                            let mut msg = match SipConnection::update_msg_received(
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

                            for h in msg.headers_mut().iter_mut() {
                                match h {
                                    rsip::Header::Contact(c) => match c.typed() {
                                        Ok(mut typed) => {
                                            typed.uri.params.retain_mut(|p| match p {
                                                rsip::Param::Other(name, _)
                                                    if name
                                                        .value()
                                                        .eq_ignore_ascii_case("rport") =>
                                                {
                                                    false
                                                }
                                                rsip::Param::Transport(_)
                                                | rsip::Param::Received(_) => false,
                                                _ => true,
                                            });
                                            typed.uri.params.push(rsip::Param::Transport(
                                                transport_type.clone(),
                                            ));
                                            typed.uri.params.push(rsip::Param::Received(
                                                client_addr.addr.ip().to_string().into(),
                                            ));
                                            typed.uri.params.push(rsip::Param::Other(
                                                rsip::param::OtherParam::new("rport"),
                                                Some(rsip::param::OtherParamValue::new(
                                                    client_addr.addr.port().to_string(),
                                                )),
                                            ));
                                            *c = typed.into();
                                        }
                                        Err(_) => {}
                                    },
                                    _ => {}
                                }
                            }

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
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    };
    let local_addr_clone = local_addr.clone();
    let write_to_websocket_loop = async move {
        while let Some(event) = to_ws_rx.recv().await {
            match event {
                TransportEvent::Incoming(sip_msg, _, _) => {
                    let message_text = sip_msg.to_string();
                    let cseq = sip_msg.cseq_header().ok();
                    debug!(
                        addr = %local_addr_clone,
                        cseq = cseq.map(|c| c.value()).unwrap_or_default(),
                        "ws forwarding \n{}",
                        message_text
                    );
                    if let Err(e) = ws_sink.send(Message::Text(message_text.into())).await {
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
    };

    select! {
        _ = token.cancelled() => {
            info!(addr = %local_addr, "WebSocket connection cancelled");
        }
        _ = read_from_websocket_loop => {}
        _ = write_to_websocket_loop => {}
    }
    ws_token.cancel();
    endpoint_ref.transport_layer.del_connection(&local_addr);
    info!(addr = %local_addr, "WebSocket connection handler exiting");
}
