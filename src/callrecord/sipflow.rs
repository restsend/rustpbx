use crate::sipflow::{SipFlowBackend, SipFlowItem, SipFlowMsgType};
use arc_swap::ArcSwap;
use bytes::Bytes;
use rsipstack::sip::{SipMessage, ToTypedHeader, prelude::HeadersExt};
use rsipstack::{transaction::endpoint::MessageInspector, transport::SipAddr};
use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    FlushSync {
        done: tokio::sync::oneshot::Sender<()>,
    },
    Shutdown,
}

struct Backend(Option<Arc<dyn SipFlowBackend>>);

/// Bounded waits so a stuck writer thread cannot block a flush (and with it
/// the query/CDR path) indefinitely.
const WRITER_FLUSH_SEND_TIMEOUT: Duration = Duration::from_secs(1);
const WRITER_FLUSH_WAIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Enqueue a `FlushSync` on the writer channel with a bounded wait
/// (`std::sync::mpsc` has no stable timed send). Returns the oneshot
/// receiver on success, `None` on timeout / disconnected writer.
async fn send_writer_flush_sync(
    tx: &SyncSender<WriteCommand>,
) -> Option<tokio::sync::oneshot::Receiver<()>> {
    let deadline = std::time::Instant::now() + WRITER_FLUSH_SEND_TIMEOUT;
    loop {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        match tx.try_send(WriteCommand::FlushSync { done: done_tx }) {
            Ok(()) => return Some(done_rx),
            Err(mpsc::TrySendError::Full(_)) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return None,
        }
    }
}

/// Await a writer flush acknowledgement with a bounded wait.
async fn await_writer_flush(rx: Option<tokio::sync::oneshot::Receiver<()>>) -> bool {
    match rx {
        Some(rx) => tokio::time::timeout(WRITER_FLUSH_WAIT_TIMEOUT, rx)
            .await
            .is_ok(),
        None => false,
    }
}

/// Maximum time a query-path flush may take before the query proceeds anyway
/// (degraded: the result may miss records still sitting in write buffers).
pub const QUERY_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// Late-bound handle to the process-wide [`SipFlow`]. Call-record hooks are
/// assembled before the SIP server (and thus the SipFlow) exists; the handle
/// is filled in right after server construction. Hooks only run after calls
/// finish — long after the handle has been set.
pub type SipFlowSlot = Arc<std::sync::OnceLock<SipFlow>>;

/// Flush a [`SipFlow`] pipeline with the query-path deadline. On timeout the
/// caller should proceed with the (possibly stale) query and log a warning.
pub async fn flush_with_deadline(sipflow: &SipFlow) {
    if tokio::time::timeout(QUERY_FLUSH_TIMEOUT, sipflow.flush())
        .await
        .is_err()
    {
        metrics::counter!("sipflow_query_flush_timeout_total", "component" => "sipflow")
            .increment(1);
    }
}

/// Flush through a late-bound [`SipFlowSlot`] handle (drains the writer batch
/// and then the backend), falling back to the bare backend when the handle
/// has not been set (tests / backends without a SipFlow wrapper).
pub async fn flush_hook_pipeline(slot: &SipFlowSlot, backend: &dyn SipFlowBackend) {
    match slot.get() {
        Some(sipflow) => flush_with_deadline(sipflow).await,
        None => {
            if tokio::time::timeout(QUERY_FLUSH_TIMEOUT, backend.flush())
                .await
                .is_err()
            {
                metrics::counter!("sipflow_query_flush_timeout_total", "component" => "sipflow")
                    .increment(1);
            }
        }
    }
}

struct SipFlowInner {
    shared_backend: Arc<ArcSwap<Backend>>,
    inspectors: Vec<Box<dyn MessageInspector>>,
    writer_tx: Option<SyncSender<WriteCommand>>,
    pool: Arc<ItemPool>,
    local_addrs: Vec<String>,
    /// Number of SIP messages dropped because the async writer channel was full.
    dropped_count: AtomicU64,
}

#[derive(Clone)]
pub struct SipFlow {
    inner: Arc<SipFlowInner>,
}

impl SipFlow {
    pub fn backend(&self) -> Option<Arc<dyn SipFlowBackend>> {
        self.inner.shared_backend.load().0.clone()
    }

    pub fn has_backend(&self) -> bool {
        self.inner.shared_backend.load().0.is_some()
    }

    /// Number of SIP messages dropped because the async writer channel was full
    /// (or the writer thread was disconnected). Exposed for call-record diagnostics.
    pub fn dropped_count(&self) -> u64 {
        self.inner.dropped_count.load(Ordering::Relaxed)
    }

    pub fn new(
        backend: Option<Arc<dyn SipFlowBackend>>,
        inspectors: Vec<Box<dyn MessageInspector>>,
        enable_async_writer: bool,
    ) -> Self {
        let pool = Arc::new(ItemPool::new());
        let pool_clone = pool.clone();

        let shared_backend = Arc::new(ArcSwap::new(Arc::new(Backend(backend))));

        let writer_tx = if enable_async_writer && shared_backend.load().0.is_some() {
            let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
            let sb_for_writer = shared_backend.clone();

            // Use dedicated OS thread instead of tokio task
            // This avoids Tokio scheduling overhead
            thread::spawn(move || {
                Self::batch_writer_thread(sb_for_writer, rx, pool_clone);
            });

            Some(tx)
        } else {
            None
        };

        SipFlow {
            inner: Arc::new(SipFlowInner {
                shared_backend,
                inspectors,
                writer_tx,
                pool,
                local_addrs: Vec::new(),
                dropped_count: AtomicU64::new(0),
            }),
        }
    }

    /// Atomically swap the backend without restarting the writer thread.
    /// The next flush in the batch writer thread will use the new backend.
    pub fn swap_backend(&self, new_backend: Arc<dyn SipFlowBackend>) {
        self.inner
            .shared_backend
            .store(Arc::new(Backend(Some(new_backend))));
    }

    /// Remove the backend entirely (disable sipflow at runtime).
    pub fn clear_backend(&self) {
        self.inner.shared_backend.store(Arc::new(Backend(None)));
    }

    /// Dedicated writer thread - avoids Tokio runtime overhead.
    /// Reads the current backend from the shared lock on each flush so that
    /// the backend can be hot-swapped at runtime via [`SipFlow::swap_backend`].
    fn batch_writer_thread(
        shared_backend: Arc<ArcSwap<Backend>>,
        rx: mpsc::Receiver<WriteCommand>,
        pool: Arc<ItemPool>,
    ) {
        let mut batch: Vec<(String, SipFlowItem, Option<usize>)> = Vec::with_capacity(BATCH_SIZE);
        let mut last_flush = std::time::Instant::now();

        loop {
            // Obtain a snapshot of the current backend on each iteration
            // so that a runtime swap takes effect within one batch cycle.
            let current_backend = shared_backend.load().0.clone();

            // Batch recv with timeout
            match rx.recv_timeout(std::time::Duration::from_millis(BATCH_FLUSH_MS)) {
                Ok(cmd) => match cmd {
                    WriteCommand::Record {
                        call_id,
                        item,
                        pool_idx,
                    } => {
                        batch.push((call_id, item, pool_idx));

                        if batch.len() >= BATCH_SIZE {
                            if let Some(ref backend) = current_backend {
                                Self::flush_batch(backend, &mut batch, &pool);
                            } else {
                                // Backend was removed while items were queued:
                                // drain the batch and release pooled items.
                                for (_, _, pool_idx) in batch.drain(..) {
                                    if let Some(idx) = pool_idx {
                                        pool.release(idx);
                                    }
                                }
                            }
                            last_flush = std::time::Instant::now();
                        }
                    }
                    WriteCommand::FlushSync { done } => {
                        if let Some(ref backend) = current_backend {
                            Self::flush_batch(backend, &mut batch, &pool);
                        } else {
                            Self::release_batch(&mut batch, &pool);
                        }
                        last_flush = std::time::Instant::now();
                        let _ = done.send(());
                    }
                    WriteCommand::Shutdown => {
                        if let Some(ref backend) = current_backend {
                            Self::flush_batch(backend, &mut batch, &pool);
                        } else {
                            Self::release_batch(&mut batch, &pool);
                        }
                        break;
                    }
                },
                Err(RecvTimeoutError::Disconnected) => {
                    // Channel closed, flush and exit
                    if let Some(ref backend) = current_backend {
                        Self::flush_batch(backend, &mut batch, &pool);
                    } else {
                        Self::release_batch(&mut batch, &pool);
                    }
                    break;
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Timeout - flush pending if needed
                    if !batch.is_empty()
                        && last_flush.elapsed().as_millis() >= BATCH_FLUSH_MS as u128
                    {
                        if let Some(ref backend) = current_backend {
                            Self::flush_batch(backend, &mut batch, &pool);
                        }
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
            let _ = backend.record(Cow::Owned(call_id), item);

            // Return item to pool
            if let Some(idx) = pool_idx {
                pool.release(idx);
            }
        }
    }

    /// Drain the batch without a backend (backend was removed / disabled at
    /// runtime): pooled items must still be returned, otherwise the pool
    /// permanently loses slots and degenerates into fresh allocations.
    #[inline]
    fn release_batch(batch: &mut Vec<(String, SipFlowItem, Option<usize>)>, pool: &Arc<ItemPool>) {
        for (_, _, pool_idx) in batch.drain(..) {
            if let Some(idx) = pool_idx {
                pool.release(idx);
            }
        }
    }

    /// Ultra-optimized record_sip with zero-copy where possible
    #[inline]
    pub fn record_sip(&self, is_outgoing: bool, msg: &SipMessage, addr: Option<&SipAddr>) {
        // Fast check: skip if no backend is configured
        if !self.has_backend() {
            return;
        }

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
                if tx
                    .try_send(WriteCommand::Record {
                        call_id,
                        item,
                        pool_idx,
                    })
                    .is_err()
                {
                    // Channel full or disconnected: count the drop so it can be
                    // surfaced in diagnostics instead of being completely silent.
                    self.inner.dropped_count.fetch_add(1, Ordering::Relaxed);
                    if let Some(idx) = pool_idx {
                        self.inner.pool.release(idx);
                    }
                }
            } else {
                // Fallback: direct synchronous write (writer_tx is None)
                if let Some(ref backend) = (*self.inner.shared_backend.load()).0 {
                    let _ = backend.record(Cow::Owned(call_id), item);
                }
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

    /// Flush the async writer batch and then the backend pipeline, waiting
    /// for each step. Ensures all recorded messages are persisted before
    /// querying the backend — this is THE entry point for post-call flushes
    /// (CDR hooks) and pre-query flushes (query endpoints).
    ///
    /// Waits are bounded so a stuck writer thread cannot block the caller
    /// indefinitely; on timeout the backend flush proceeds anyway
    /// (best-effort, counted).
    pub async fn flush(&self) {
        if let Some(ref tx) = self.inner.writer_tx {
            if !await_writer_flush(send_writer_flush_sync(tx).await).await {
                metrics::counter!("sipflow_writer_flush_timeout_total", "component" => "sipflow")
                    .increment(1);
            }
        }
        if let Some(ref backend) = (*self.inner.shared_backend.load()).0 {
            let _ = backend.flush().await;
        }
    }

    /// Synchronously flush the batch writer and wait for completion.
    /// Ensures all recorded messages are persisted before querying the backend.
    pub async fn flush_sync(&self) {
        self.flush().await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SipFlowSubdirs;
    use crate::sipflow::backend::local::LocalBackend;

    const CALL_ID: &str = "sipflow-flush-regression-call";

    fn sip_request(call_id: &str, cseq: u32) -> rsipstack::sip::SipMessage {
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK{cseq}\r\n\
             From: <sip:alice@example.com>;tag=tag{cseq}\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} INVITE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        );
        rsipstack::sip::SipMessage::try_from(raw).expect("valid SIP request")
    }

    /// flush_count / flush_interval are set high so nothing is persisted
    /// unless an explicit flush drains the pipeline.
    fn test_backend(root: &std::path::Path) -> Arc<dyn SipFlowBackend> {
        Arc::new(
            LocalBackend::new(
                root.to_string_lossy().into_owned(),
                SipFlowSubdirs::None,
                1000,
                3600,
                128,
                None,
                2,
                false,
                16000,
                false,
            )
            .expect("local backend"),
        )
    }

    async fn query_count(backend: &Arc<dyn SipFlowBackend>, call_id: &str) -> usize {
        let now = chrono::Local::now();
        backend
            .query_flow(
                call_id,
                now - chrono::Duration::hours(1),
                now + chrono::Duration::hours(1),
            )
            .await
            .expect("query flow")
            .len()
    }

    /// Core regression for the post-call query race: CDR hooks and query
    /// endpoints flush through `SipFlow::flush`, which must drain the async
    /// writer thread — including the last messages still sitting in its 50ms
    /// batch — before flushing the backend pipeline.
    #[tokio::test]
    async fn sipflow_flush_drains_writer_and_backend_before_query() {
        let dir = tempfile::tempdir().unwrap();
        let backend = test_backend(dir.path());
        let sipflow = SipFlow::new(Some(backend.clone()), Vec::new(), true);

        for cseq in 1..=5u32 {
            sipflow.record_sip(false, &sip_request(CALL_ID, cseq), None);
        }

        // Simulate the post-call query path: flush exactly like
        // SipFlowUploadHook / the endpoints do (via the SipFlow wrapper).
        sipflow.flush().await;

        assert_eq!(
            query_count(&backend, CALL_ID).await,
            5,
            "every recorded message must be visible after SipFlow::flush"
        );
    }

    /// Hot-reload: `swap_backend` installs a fresh backend instance. The
    /// SipFlow wrapper is unchanged, so its flush must keep draining the
    /// writer thread into the *current* backend.
    #[tokio::test]
    async fn flush_after_swap_backend_targets_new_backend() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let b1 = test_backend(dir1.path());
        let sipflow = SipFlow::new(Some(b1), Vec::new(), true);

        sipflow.record_sip(false, &sip_request(CALL_ID, 1), None);

        let b2 = test_backend(dir2.path());
        sipflow.swap_backend(b2.clone());
        for cseq in 2..=4u32 {
            sipflow.record_sip(false, &sip_request(CALL_ID, cseq), None);
        }

        sipflow.flush().await;

        // Message 1 was still queued in the writer batch when the swap
        // happened; the writer flushes its batch into the *current* backend,
        // so all four messages end up visible through b2.
        assert_eq!(
            query_count(&b2, CALL_ID).await,
            4,
            "flush after swap must drain the writer into the new backend"
        );
    }

    /// Sync-writer mode (enable_async_writer = false) records directly into
    /// the backend; SipFlow::flush must still make the messages visible.
    #[tokio::test]
    async fn sync_writer_records_visible_after_sipflow_flush() {
        let dir = tempfile::tempdir().unwrap();
        let backend = test_backend(dir.path());
        let sipflow = SipFlow::new(Some(backend.clone()), Vec::new(), false);

        for cseq in 1..=3u32 {
            sipflow.record_sip(false, &sip_request(CALL_ID, cseq), None);
        }

        sipflow.flush().await;

        assert_eq!(query_count(&backend, CALL_ID).await, 3);
    }
}
