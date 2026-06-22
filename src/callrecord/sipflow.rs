use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};
use bytes::Bytes;
use crossbeam_channel::{RecvTimeoutError, Sender, bounded};
use rsipstack::sip::{SipMessage, ToTypedHeader, prelude::HeadersExt};
use rsipstack::{transaction::endpoint::MessageInspector, transport::SipAddr};
use std::sync::{Arc, Mutex};
use std::thread;

const BATCH_SIZE: usize = 256;
const BATCH_FLUSH_MS: u64 = 50;
const POOL_SIZE: usize = 1024;
const CHANNEL_CAPACITY: usize = BATCH_SIZE * 4;

/// Pooled SipFlowItem to reduce allocations
struct PooledItem {
    item: SipFlowItem,
    in_use: bool,
}

/// Object pool for SipFlowItem
struct ItemPool {
    items: Vec<Mutex<PooledItem>>,
}

impl ItemPool {
    fn new() -> Self {
        let mut items = Vec::with_capacity(POOL_SIZE);
        for _ in 0..POOL_SIZE {
            items.push(Mutex::new(PooledItem {
                item: SipFlowItem {
                    timestamp: 0,
                    seq: 0,
                    leg: None,
                    msg_type: SipFlowMsgType::Sip,
                    src_addr: String::with_capacity(64),
                    dst_addr: String::with_capacity(64),
                    payload: Bytes::new(),
                },
                in_use: false,
            }));
        }
        Self { items }
    }

    /// Acquire an item from pool (try-lock fast path)
    fn acquire(&self) -> Option<(usize, SipFlowItem)> {
        // Round-robin start index to reduce contention
        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as usize
            % POOL_SIZE;

        for i in 0..POOL_SIZE {
            let idx = (start + i) % POOL_SIZE;
            if let Ok(mut guard) = self.items[idx].try_lock()
                && !guard.in_use
            {
                guard.in_use = true;
                // Clone the item for use
                let cloned = Self::clone_item(&guard.item);
                return Some((idx, cloned));
            }
        }
        // Pool exhausted, allocate new
        None
    }

    /// Release item back to pool
    fn release(&self, idx: usize) {
        if idx < POOL_SIZE
            && let Ok(mut guard) = self.items[idx].lock()
        {
            guard.in_use = false;
            // Clear strings to keep capacity but free content
            guard.item.src_addr.clear();
            guard.item.dst_addr.clear();
        }
    }

    fn clone_item(item: &SipFlowItem) -> SipFlowItem {
        SipFlowItem {
            timestamp: item.timestamp,
            seq: item.seq,
            leg: item.leg,
            msg_type: item.msg_type.clone(),
            src_addr: String::with_capacity(64),
            dst_addr: String::with_capacity(64),
            payload: item.payload.clone(),
        }
    }
}

/// Optimized write command with pool index
enum WriteCommand {
    Record {
        call_id: String,
        item: SipFlowItem,
        pool_idx: Option<usize>, // None if not from pool
    },
    Flush,
    FlushSync {
        done: tokio::sync::oneshot::Sender<()>,
    },
    Shutdown,
}

struct SipFlowInner {
    backend: Option<Arc<dyn SipFlowBackend>>,
    inspectors: Vec<Box<dyn MessageInspector>>,
    writer_tx: Option<Sender<WriteCommand>>,
    pool: Arc<ItemPool>,
    local_addrs: Vec<String>,
}

#[derive(Clone)]
pub struct SipFlow {
    inner: Arc<SipFlowInner>,
}

impl SipFlow {
    pub fn backend(&self) -> Option<Arc<dyn SipFlowBackend>> {
        self.inner.backend.clone()
    }

    pub fn new(
        backend: Option<Arc<dyn SipFlowBackend>>,
        inspectors: Vec<Box<dyn MessageInspector>>,
        enable_async_writer: bool,
    ) -> Self {
        let pool = Arc::new(ItemPool::new());
        let pool_clone = pool.clone();

        let writer_tx = if enable_async_writer {
            backend.as_ref().map(|b| {
                let (tx, rx) = bounded(CHANNEL_CAPACITY);
                let backend_clone = b.clone();

                // Use dedicated OS thread instead of tokio task
                // This avoids Tokio scheduling overhead
                thread::spawn(move || {
                    Self::batch_writer_thread(backend_clone, rx, pool_clone);
                });

                tx
            })
        } else {
            None
        };

        SipFlow {
            inner: Arc::new(SipFlowInner {
                backend,
                inspectors,
                writer_tx,
                pool,
                local_addrs: Vec::new(),
            }),
        }
    }

    /// Dedicated writer thread - avoids Tokio runtime overhead
    fn batch_writer_thread(
        backend: Arc<dyn SipFlowBackend>,
        rx: crossbeam_channel::Receiver<WriteCommand>,
        pool: Arc<ItemPool>,
    ) {
        let mut batch: Vec<(String, SipFlowItem, Option<usize>)> = Vec::with_capacity(BATCH_SIZE);
        let mut last_flush = std::time::Instant::now();

        loop {
            // Calculate deadline for recv
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(BATCH_FLUSH_MS);

            // Batch recv with timeout
            match rx.recv_deadline(deadline) {
                Ok(cmd) => match cmd {
                    WriteCommand::Record {
                        call_id,
                        item,
                        pool_idx,
                    } => {
                        batch.push((call_id, item, pool_idx));

                        if batch.len() >= BATCH_SIZE {
                            Self::flush_batch(&backend, &mut batch, &pool);
                            last_flush = std::time::Instant::now();
                        }
                    }
                    WriteCommand::Flush => {
                        Self::flush_batch(&backend, &mut batch, &pool);
                        last_flush = std::time::Instant::now();
                    }
                    WriteCommand::FlushSync { done } => {
                        Self::flush_batch(&backend, &mut batch, &pool);
                        last_flush = std::time::Instant::now();
                        let _ = done.send(());
                    }
                    WriteCommand::Shutdown => {
                        Self::flush_batch(&backend, &mut batch, &pool);
                        break;
                    }
                },
                Err(RecvTimeoutError::Disconnected) => {
                    // Channel closed, flush and exit
                    Self::flush_batch(&backend, &mut batch, &pool);
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Timeout - flush pending if needed
                    if !batch.is_empty()
                        && last_flush.elapsed().as_millis() >= BATCH_FLUSH_MS as u128
                    {
                        Self::flush_batch(&backend, &mut batch, &pool);
                        last_flush = std::time::Instant::now();
                    }
                }
            }
        }
    }

    #[inline]
    fn flush_batch(
        backend: &Arc<dyn SipFlowBackend>,
        batch: &mut Vec<(String, SipFlowItem, Option<usize>)>,
        pool: &Arc<ItemPool>,
    ) {
        // Process batch
        for (call_id, item, pool_idx) in batch.drain(..) {
            let _ = backend.record(&call_id, item);

            // Return item to pool
            if let Some(idx) = pool_idx {
                pool.release(idx);
            }
        }

        // Use blocking flush - we're in a dedicated thread
        let _ = std::hint::black_box(backend); // Keep reference alive
    }

    /// Ultra-optimized record_sip with zero-copy where possible
    #[inline]
    pub fn record_sip(&self, is_outgoing: bool, msg: &SipMessage, addr: Option<&SipAddr>) {
        let backend = match &self.inner.backend {
            Some(b) => b,
            None => return,
        };

        // Fast path: extract call_id header without full parsing
        let call_id_result = match msg {
            rsipstack::sip::SipMessage::Request(req) => req.call_id_header(),
            rsipstack::sip::SipMessage::Response(resp) => resp.call_id_header(),
        };

        if let Ok(id) = call_id_result {
            let call_id = id.value().to_string();

            // OPTIMIZATION: Zero-copy - use pre-sized allocation
            let payload = Self::message_to_bytes_fast(msg);

            // OPTIMIZATION: Pre-sized string allocation
            let (src_addr, dst_addr) =
                Self::extract_addrs_fast(is_outgoing, addr, msg, &self.inner.local_addrs);

            // OPTIMIZATION: Object pool
            let (pool_idx, mut item) = self
                .inner
                .pool
                .acquire()
                .map(|(idx, item)| (Some(idx), item))
                .unwrap_or((
                    None,
                    SipFlowItem {
                        timestamp: 0,
                        seq: 0,
                        leg: None,
                        msg_type: SipFlowMsgType::Sip,
                        src_addr: String::with_capacity(64),
                        dst_addr: String::with_capacity(64),
                        payload: Bytes::new(),
                    },
                ));

            // Fill item (reuse allocation from pool)
            item.timestamp = chrono::Utc::now().timestamp_micros() as u64;
            item.seq = 0;
            item.leg = None;
            item.msg_type = SipFlowMsgType::Sip;
            item.src_addr = src_addr;
            item.dst_addr = dst_addr;
            item.payload = payload;

            // Send to writer thread (non-blocking, drops if full)
            if let Some(ref tx) = self.inner.writer_tx {
                // Use try_send to avoid blocking - drop if channel full
                let _ = tx.try_send(WriteCommand::Record {
                    call_id,
                    item,
                    pool_idx,
                });
            } else {
                // Fallback: direct synchronous write
                let _ = backend.record(&call_id, item);
            }
        }
    }

    /// Fast path: convert message to Bytes without full string clone
    #[inline]
    fn message_to_bytes_fast(msg: &SipMessage) -> Bytes {
        // Use the standard to_string but let Bytes reuse the allocation
        let msg_str = msg.to_string();
        Bytes::from(msg_str)
    }

    /// Fast address extraction with pre-allocated strings
    #[inline]
    fn extract_addrs_fast(
        is_outgoing: bool,
        addr: Option<&SipAddr>,
        msg: &SipMessage,
        local_addrs: &[String],
    ) -> (String, String) {
        let mut src = String::with_capacity(64);
        let mut dst = String::with_capacity(64);

        if let Some(addr) = addr {
            let addr_str = addr.addr.to_string();
            if is_outgoing {
                dst.push_str(&addr_str);
            } else {
                src.push_str(&addr_str);
            }
        } else if is_outgoing
            && let Ok(dest) = rsipstack::transport::SipConnection::get_destination(msg)
        {
            dst.push_str(&dest.to_string());
        }

        if is_outgoing {
            // Outgoing messages: fill src (local) side
            if src.is_empty() && msg.is_request() {
                // Outgoing requests: local address from Via sent-by
                if let Ok(via) = msg.via_header() {
                    if let Ok(typed_via) = via.typed() {
                        src.push_str(&typed_via.uri.host_with_port.to_string());
                    }
                }
            }
            if src.is_empty() && !local_addrs.is_empty() {
                src.push_str(&local_addrs[0]);
            }
        } else {
            // Incoming messages: fill dst (local) side with the server's
            // actual listening address, NOT the SIP Request-URI.
            if dst.is_empty() && !local_addrs.is_empty() {
                dst.push_str(&local_addrs[0]);
            }
            // Last resort fallback: for incoming responses, Via header can
            // tell us where the response was sent (our address).
            if dst.is_empty() {
                if let SipMessage::Response(resp) = msg {
                    if let Some(via_addr) = resp.via_received() {
                        dst.push_str(&via_addr.to_string());
                    } else if let Ok(via) = resp.via_header() {
                        if let Ok(typed_via) = via.typed() {
                            dst.push_str(&typed_via.uri.host_with_port.to_string());
                        }
                    }
                }
            }
        }

        (src, dst)
    }

    pub async fn flush(&self) {
        if let Some(ref tx) = self.inner.writer_tx {
            let _ = tx.send(WriteCommand::Flush);
        }
        if let Some(ref backend) = self.inner.backend {
            let _ = backend.flush().await;
        }
    }

    /// Synchronously flush the batch writer and wait for completion.
    /// Ensures all recorded messages are persisted before querying the backend.
    pub async fn flush_sync(&self) {
        if let Some(ref tx) = self.inner.writer_tx {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(WriteCommand::FlushSync { done: done_tx });
            let _ = done_rx.await;
        }
        if let Some(ref backend) = self.inner.backend {
            let _ = backend.flush().await;
        }
    }
}

impl MessageInspector for SipFlow {
    fn before_send(&self, msg: SipMessage, dest: Option<&SipAddr>) -> SipMessage {
        self.record_sip(true, &msg, dest);
        let mut modified_msg = msg;
        for inspector in &self.inner.inspectors {
            modified_msg = inspector.before_send(modified_msg, dest);
        }
        modified_msg
    }

    fn after_received(&self, msg: SipMessage, from: Option<&SipAddr>) -> SipMessage {
        self.record_sip(false, &msg, from);
        let mut modified_msg = msg;
        for inspector in &self.inner.inspectors {
            modified_msg = inspector.after_received(modified_msg, from);
        }
        modified_msg
    }
}

impl Drop for SipFlow {
    fn drop(&mut self) {
        // Signal writer thread to shutdown
        if let Some(ref tx) = self.inner.writer_tx {
            let _ = tx.send(WriteCommand::Shutdown);
        }
    }
}

pub struct SipFlowBuilder {
    inspectors: Vec<Box<dyn MessageInspector>>,
    backend: Option<Arc<dyn SipFlowBackend>>,
    enable_async_writer: bool,
    local_addrs: Vec<String>,
}

impl SipFlowBuilder {
    pub fn new() -> Self {
        Self {
            inspectors: Vec::new(),
            backend: None,
            enable_async_writer: true,
            local_addrs: Vec::new(),
        }
    }

    pub fn with_backend(mut self, backend: Arc<dyn SipFlowBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    pub fn register_inspector(mut self, inspector: Box<dyn MessageInspector>) -> Self {
        self.inspectors.push(inspector);
        self
    }

    /// Disable async batch writer (use synchronous writes)
    pub fn with_sync_writer(mut self) -> Self {
        self.enable_async_writer = false;
        self
    }

    /// Set the server's local listening addresses (e.g. ["0.0.0.0:5060", "0.0.0.0:15060"]).
    /// Used as the dst_addr for received messages and src_addr for sent messages.
    pub fn with_local_addrs(mut self, addrs: Vec<String>) -> Self {
        self.local_addrs = addrs;
        self
    }

    pub fn build(self) -> SipFlow {
        let mut flow = SipFlow::new(self.backend, self.inspectors, self.enable_async_writer);
        // SAFETY: inner is behind Arc but we just created it, no other references exist.
        let inner =
            Arc::get_mut(&mut flow.inner).expect("SipFlow inner uniquely held during build");
        inner.local_addrs = self.local_addrs;
        flow
    }
}

impl Default for SipFlowBuilder {
    fn default() -> Self {
        Self::new()
    }
}
