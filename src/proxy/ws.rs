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

/// A bridged WS session with no inbound frames for this long is torn down:
/// clients auto-pong our 25s pings, so silence means the connection is
/// half-open (client vanished without FIN — network drop, NAT rebinding,
/// nginx killing the client leg without closing upstream cleanly). Without
/// this bound, half-open sockets pin the handler task and the transport
/// table entry for as long as the kernel tolerates the dead TCP peer.
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// RAII cleanup for a bridged WebSocket session: drops of the handler future
/// (axum/hyper tearing the connection task down mid-flight — client races,
/// `nginx -s reload` killing worker connections in bulk) and panics inside
/// the loops would otherwise skip the cleanup after the `select!`, leaving
/// the transport-table entry behind. Every exit path — normal completion,
/// future drop, unwind — runs this `Drop`.
///
/// WARNING: hold the `EndpointInnerRef` here — NEVER a
/// `rsipstack::transport::TransportLayer` clone. rsipstack implements
/// `Drop for TransportLayer` by cancelling the *shared* cancel token, and
/// every clone shares the same `Arc<TransportLayerInner>`/token: a guard that
/// owned a clone tore down the whole SIP stack (all listeners and serve
/// loops) the first time any WS session ended. `EndpointInnerRef` is a plain
/// `Arc` with no `Drop` side effects and reaches the same `TransportLayer`.
struct WsChannelCleanupGuard {
    endpoint: EndpointInnerRef,
    sip_connection: SipConnection,
    ws_token: CancellationToken,
    addr: SipAddr,
}

impl Drop for WsChannelCleanupGuard {
    fn drop(&mut self) {
        // Idempotent by construction: token cancellation is idempotent and
        // `remove_connection` is a no-op once the entry is gone.
        self.ws_token.cancel();
        self.endpoint
            .transport_layer
            .remove_connection(&self.sip_connection);
        debug!(addr = %self.addr, "ws channel cleanup: transport entry removed");
    }
}

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

    // From here on, every exit path (client Close/EOF, ping failure, idle
    // timeout, token cancellation, handler future being dropped mid-flight,
    // panic unwind) runs the cleanup — the transport-table entry cannot
    // outlive this future. The guard is intentionally unused: it exists so
    // its Drop runs when this future ends.
    let _cleanup_guard = WsChannelCleanupGuard {
        endpoint: endpoint_ref.clone(),
        sip_connection: sip_connection.clone(),
        ws_token: ws_token.clone(),
        addr: local_addr.clone(),
    };

    const WS_PING_INTERVAL: Duration = Duration::from_secs(25);
    let local_addr_clone = local_addr.clone();
    let sip_connection_clone = sip_connection.clone();
    let read_from_websocket_loop = async move {
        loop {
            // Bound the wait for the next inbound frame: a healthy client
            // answers our 25s pings with auto-pongs, so 90s of silence means
            // the connection is half-open — tear it down instead of pinning
            // the handler task (and the transport entry) for hours.
            let next_frame =
                tokio::time::timeout(WS_IDLE_TIMEOUT, ws_read.next()).await;
            match next_frame {
                Err(_elapsed) => {
                    debug!(
                        addr = %local_addr_clone,
                        idle_secs = WS_IDLE_TIMEOUT.as_secs(),
                        "WebSocket idle timeout, closing connection"
                    );
                    break;
                }
                Ok(None) => {
                    debug!(addr = %local_addr_clone, "WebSocket stream ended");
                    break;
                }
                Ok(Some(Err(e))) => {
                    debug!(addr = %local_addr_clone, "WebSocket read error: {}", e);
                    break;
                }
                Ok(Some(Ok(message))) => {
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
    // Cleanup is owned by `cleanup_guard` (Drop): it runs here on normal
    // completion as well as when the handler future is dropped mid-flight or
    // unwinds through a panic — the paths that used to skip this block and
    // leak the transport-table entry.
}

#[cfg(test)]
mod channel_cleanup_tests {
    use super::*;
    use rsipstack::transaction::endpoint::EndpointBuilder;
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

    /// The handler future can be dropped mid-flight (hyper tears the
    /// connection task down on a client race, `nginx -s reload` kills worker
    /// connections in bulk): the `select!` at the tail of `sip_ws_handler`
    /// never completes and the pre-guard code skipped cleanup entirely. The
    /// guard's `Drop` must reclaim the transport-table entry on that path.
    #[tokio::test]
    async fn cleanup_guard_reclaims_entry_when_handler_future_is_dropped() {
        let endpoint = EndpointBuilder::new()
            .with_transport_layer(TransportLayer::new(CancellationToken::new()))
            .build();
        let tl = &endpoint.inner.transport_layer;
        let peer_addr = SipAddr {
            r#type: Some(transport_type_for_test()),
            addr: "203.0.113.7:54322"
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
        let sip_connection = SipConnection::Channel(connection);
        tl.add_connection(sip_connection.clone());
        assert_eq!(tl.connections_count(), 1);

        let token = CancellationToken::new();
        let ws_token = token.child_token();
        // Simulate the aborted handler: only the guard survives the drop.
        let guard = WsChannelCleanupGuard {
            endpoint: endpoint.inner.clone(),
            sip_connection: sip_connection.clone(),
            ws_token: ws_token.clone(),
            addr: peer_addr,
        };
        drop(guard);

        assert_eq!(
            tl.connections_count(),
            0,
            "guard must reclaim the entry when the handler future is dropped"
        );
        assert!(
            ws_token.is_cancelled(),
            "guard must cancel the session token so flow affinity stops routing to it"
        );
    }

    /// Regression test for the premature-shutdown bug: rsipstack implements
    /// `Drop for TransportLayer` by cancelling the *shared* cancel token, and
    /// every clone shares the same `Arc<TransportLayerInner>`/token. A cleanup
    /// guard that owned a `TransportLayer` clone therefore tore down the whole
    /// SIP stack (all listeners and serve loops) the first time any WS session
    /// ended. The guard must reach the transport layer through the
    /// `EndpointInnerRef` (a plain `Arc` with no `Drop` side effects) instead:
    /// dropping it reclaims the table entry and cancels only the session
    /// token, while the shared transport token stays live.
    #[tokio::test]
    async fn cleanup_guard_does_not_cancel_shared_transport_token() {
        let token = CancellationToken::new();
        let endpoint = EndpointBuilder::new()
            .with_transport_layer(TransportLayer::new(token.clone()))
            .build();
        let tl = &endpoint.inner.transport_layer;

        let peer_addr = SipAddr {
            r#type: Some(transport_type_for_test()),
            addr: "203.0.113.7:54323"
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
        let sip_connection = SipConnection::Channel(connection);
        tl.add_connection(sip_connection.clone());
        assert_eq!(tl.connections_count(), 1);

        let ws_token = token.child_token();
        let guard = WsChannelCleanupGuard {
            endpoint: endpoint.inner.clone(),
            sip_connection,
            ws_token: ws_token.clone(),
            addr: peer_addr,
        };
        drop(guard);

        assert!(
            !token.is_cancelled(),
            "guard drop cancelled the shared transport token — the whole SIP stack would shut down on the first WS session end"
        );
        assert_eq!(tl.connections_count(), 0);
        assert!(
            ws_token.is_cancelled(),
            "guard must still cancel the per-session token"
        );
    }
}
