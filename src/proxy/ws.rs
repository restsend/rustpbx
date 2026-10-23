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
use tracing::{debug, info, trace, warn};

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
                                    // Full SIP dumps are extremely chatty on a
                                    // busy proxy; TRACE keeps them available
                                    // without flooding DEBUG.
                                    trace!(
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
                                let msg = match SipConnection::update_msg_received(
                                    sip_msg,
                                    client_addr.addr,
                                    transport_type,
                                ) {
                                    Ok(msg) => msg,
                                    Err(e) => {
                                        warn!(addr = %local_addr_clone, "error updating binary SIP via: {}", e);
                                        continue;
                                    }
                                };
                                if let Err(e) = from_ws_tx.send(TransportEvent::Incoming(
                                    msg,
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
                            trace!(
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
    // Identity-based removal: only THIS connection is taken out of the
    // transport cache. Requires the local rsipstack fork (patched via
    // [patch.crates-io]), whose `retire_connection` includes Channel
    // bridges in identity-based removal — upstream 0.7.7 silently skips
    // non-stream connections here, which made the connection table grow
    // with every WS session ever served. Sibling connections sharing the
    // peer ip:port (several agents behind one office NAT legitimately do)
    // are preserved, unlike an address-scoped `del_connection`.
    endpoint_ref
        .transport_layer
        .remove_connection(&sip_connection);
}

#[cfg(test)]
mod channel_cleanup_tests {
    use super::*;
    use rsipstack::transport::TransportLayer;

    /// Regression test for the transport-connection table leak: every axum-WS
    /// session is bridged as a `SipConnection::Channel` and registered in the
    /// transport layer's connection table keyed by the peer address. When the
    /// socket ends, identity-based `remove_connection` MUST remove the entry.
    /// Upstream rsipstack 0.7.7 silently skips Channel connections in
    /// `retire_connection` (non-stream early return), which made
    /// `connections_count` grow with every WS session ever served; the local
    /// fork ([patch.crates-io]) includes Channel in identity-based removal.
    /// This test replays the exact add/cleanup sequence from
    /// `sip_ws_handler`.
    #[tokio::test]
    async fn channel_connection_removed_from_transport_table_on_cleanup() {
        let tl = TransportLayer::new(CancellationToken::new());

        // Same construction as sip_ws_handler: the peer addr (browser
        // ip:ephemeral-port) is what `add_connection` will use as the map key.
        let peer_addr = SipAddr {
            r#type: Some(transport_type_for_test()),
            addr: "203.0.113.7:54321"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        };
        let (_from_ws_tx, from_ws_rx) = mpsc::unbounded_channel();
        let (to_ws_tx, _to_ws_rx) = mpsc::unbounded_channel();
        let connection = ChannelConnection::create_connection(
            from_ws_rx,
            to_ws_tx,
            peer_addr.clone(),
            None,
        )
        .await
        .unwrap();

        // Register — exactly what ws.rs does on every accepted WS session.
        tl.add_connection(SipConnection::Channel(connection.clone()));
        assert_eq!(tl.connections_count(), 1, "channel entry not tracked");

        // Cleanup — the fixed identity-based path (fork): only this
        // connection's entry is removed.
        tl.remove_connection(&SipConnection::Channel(connection));
        assert_eq!(
            tl.connections_count(),
            0,
            "channel entry leaked after remove_connection — transport table would grow unbounded"
        );
    }

    /// The reason identity-based removal is used instead of the deprecated
    /// address-scoped `del_connection`: when a new session reuses one remote
    /// ip:port (NAT port reuse), `add_connection` OVERWRITES the cached entry
    /// (connectionless slot, single handle per peer). The OLD connection's
    /// cleanup must then NOT evict the NEW one that replaced it — identity
    /// matching fails silently and the replacement survives. An
    /// address-scoped delete would tear the live replacement down.
    #[tokio::test]
    async fn channel_replacement_survives_stale_connection_cleanup() {
        let tl = TransportLayer::new(CancellationToken::new());
        let shared_addr = SipAddr {
            r#type: Some(transport_type_for_test()),
            addr: "198.51.100.9:5060"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        };

        // Old session registers first.
        let (_from_tx_a, from_rx_a) = mpsc::unbounded_channel();
        let (to_tx_a, _to_rx_a) = mpsc::unbounded_channel();
        let old_conn = ChannelConnection::create_connection(
            from_rx_a,
            to_tx_a,
            shared_addr.clone(),
            None,
        )
        .await
        .unwrap();
        tl.add_connection(SipConnection::Channel(old_conn.clone()));
        assert_eq!(tl.connections_count(), 1);

        // NAT port reuse: a new session takes over the same remote ip:port.
        let (_from_tx_b, from_rx_b) = mpsc::unbounded_channel();
        let (to_tx_b, _to_rx_b) = mpsc::unbounded_channel();
        let new_conn = ChannelConnection::create_connection(
            from_rx_b,
            to_tx_b,
            shared_addr.clone(),
            None,
        )
        .await
        .unwrap();
        tl.add_connection(SipConnection::Channel(new_conn.clone()));
        // Connectionless slot: single handle per peer — the newcomer replaced
        // the old entry (still exactly one slot).
        assert_eq!(tl.connections_count(), 1);

        // The OLD session now ends and cleans up. Identity-based removal
        // must leave the replacement alone (an address-scoped delete would
        // evict it and blackhole the live session).
        tl.remove_connection(&SipConnection::Channel(old_conn));
        assert_eq!(
            tl.connections_count(),
            1,
            "stale connection cleanup must not evict its live replacement"
        );

        // The new session ends later: the slot is finally reclaimed.
        tl.remove_connection(&SipConnection::Channel(new_conn));
        assert_eq!(tl.connections_count(), 0);
    }

    fn transport_type_for_test() -> rsipstack::sip::transport::Transport {
        rsipstack::sip::transport::Transport::Ws
    }
}
