use crate::call::domain::{Leg, LegId};
use crate::call::runtime::conference_media_bridge::ConferenceBridgeHandle;
use crate::proxy::proxy_call::media_peer::MediaPeer;
use rsipstack::dialog::dialog::Dialog;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinHandle;

pub struct LegRegistry {
    pub states: HashMap<LegId, Leg>,
    pub dialogs: HashMap<LegId, Dialog>,
    pub peers: HashMap<LegId, Arc<dyn MediaPeer>>,
    pub transports: HashMap<LegId, rustrtc::TransportMode>,
    pub answers: HashMap<LegId, String>,
    pub has_video: HashMap<LegId, bool>,
    pub tasks: HashMap<LegId, Vec<JoinHandle<()>>>,
    pub conference_bridge_handles: HashMap<LegId, ConferenceBridgeHandle>,
}

impl LegRegistry {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
            dialogs: HashMap::new(),
            peers: HashMap::new(),
            transports: HashMap::new(),
            answers: HashMap::new(),
            has_video: HashMap::new(),
            tasks: HashMap::new(),
            conference_bridge_handles: HashMap::new(),
        }
    }

    pub fn add_leg(
        &mut self,
        id: LegId,
        state: Leg,
        peer: Arc<dyn MediaPeer>,
        dialog: Option<Dialog>,
    ) {
        self.states.insert(id.clone(), state);
        if let Some(dlg) = dialog {
            self.dialogs.insert(id.clone(), dlg);
        }
        self.peers.insert(id, peer);
    }

    pub fn remove_leg(&mut self, id: &LegId) -> Option<(Leg, Vec<JoinHandle<()>>)> {
        let state = self.states.remove(id)?;
        self.dialogs.remove(id);
        self.peers.remove(id);
        self.transports.remove(id);
        self.answers.remove(id);
        self.has_video.remove(id);
        if let Some(handle) = self.conference_bridge_handles.remove(id) {
            handle.stop();
        }
        let tasks = self.tasks.remove(id).unwrap_or_default();
        Some((state, tasks))
    }

    pub fn active_count(&self) -> usize {
        self.states.values().filter(|l| l.is_active()).count()
    }

    pub fn set_dialog(&mut self, id: LegId, dialog: Dialog) {
        self.dialogs.insert(id, dialog);
    }

    pub fn retain_dialogs_by_dialog_id(&mut self, terminated_id: &rsipstack::dialog::DialogId) {
        self.dialogs.retain(|_, dlg| dlg.id() != *terminated_id);
    }

    pub fn get_peer(&self, id: &LegId) -> Option<&Arc<dyn MediaPeer>> {
        self.peers.get(id)
    }

    pub fn set_peer(&mut self, id: LegId, peer: Arc<dyn MediaPeer>) {
        self.peers.insert(id, peer);
    }

    pub fn caller_peer(&self) -> Option<&Arc<dyn MediaPeer>> {
        self.peers.get(&LegId::new("caller"))
    }

    pub fn callee_peer(&self) -> Option<&Arc<dyn MediaPeer>> {
        self.peers.get(&LegId::new("callee"))
    }

    pub fn get_transport(&self, id: &LegId) -> Option<rustrtc::TransportMode> {
        self.transports.get(id).cloned()
    }

    pub fn set_transport(&mut self, id: LegId, transport: rustrtc::TransportMode) {
        self.transports.insert(id, transport);
    }

    pub fn caller_is_webrtc(&self) -> bool {
        self.transports
            .get(&LegId::new("caller"))
            .map(|t| *t == rustrtc::TransportMode::WebRtc)
            .unwrap_or(false)
    }

    pub fn callee_is_webrtc(&self) -> bool {
        self.transports
            .get(&LegId::new("callee"))
            .map(|t| *t == rustrtc::TransportMode::WebRtc)
            .unwrap_or(false)
    }

    pub fn get_answer(&self, id: &LegId) -> Option<&str> {
        self.answers.get(id).map(|s| s.as_str())
    }

    pub fn set_answer(&mut self, id: LegId, answer: String) {
        self.answers.insert(id, answer);
    }

    pub fn leg_has_video(&self, id: &LegId) -> bool {
        self.has_video.get(id).copied().unwrap_or(false)
    }

    pub fn set_video_state(&mut self, id: &LegId, has_video: bool) {
        self.has_video.insert(id.clone(), has_video);
    }

    pub fn set_bridge_video_state(&mut self, source: &LegId, target: &LegId, has_video: bool) {
        self.has_video.insert(source.clone(), has_video);
        self.has_video.insert(target.clone(), has_video);
    }

    pub fn any_has_video(&self) -> bool {
        self.has_video.values().any(|v| *v)
    }

    pub fn push_task(&mut self, id: LegId, handle: JoinHandle<()>) {
        self.tasks.entry(id).or_default().push(handle);
    }

    pub fn drain_tasks(&mut self) -> impl Iterator<Item = (LegId, Vec<JoinHandle<()>>)> + '_ {
        self.tasks.drain()
    }

    pub fn set_conference_bridge_handle(&mut self, id: LegId, handle: ConferenceBridgeHandle) {
        if let Some(old) = self.conference_bridge_handles.insert(id.clone(), handle) {
            old.stop();
        }
    }

    pub fn remove_conference_bridge_handle(
        &mut self,
        id: &LegId,
    ) -> Option<ConferenceBridgeHandle> {
        self.conference_bridge_handles.remove(id)
    }

    pub fn stop_all_conference_bridge_handles(&mut self) {
        for (_, handle) in self.conference_bridge_handles.drain() {
            handle.stop();
        }
    }

    pub fn contains_key(&self, id: &LegId) -> bool {
        self.states.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&LegId, &Leg)> {
        self.states.iter()
    }

    pub fn get(&self, id: &LegId) -> Option<&Leg> {
        self.states.get(id)
    }

    pub fn get_mut(&mut self, id: &LegId) -> Option<&mut Leg> {
        self.states.get_mut(id)
    }

    pub fn values(&self) -> impl Iterator<Item = &Leg> {
        self.states.values()
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Leg> {
        self.states.values_mut()
    }

    pub fn keys(&self) -> impl Iterator<Item = &LegId> {
        self.states.keys()
    }

    pub fn insert(&mut self, id: LegId, state: Leg) {
        self.states.insert(id, state);
    }

    pub fn remove(&mut self, id: &LegId) -> Option<Leg> {
        let (state, tasks) = self.remove_leg(id)?;
        for handle in tasks {
            handle.abort();
        }
        Some(state)
    }
}

impl Default for LegRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LegRegistry {
    fn drop(&mut self) {
        for (_, handles) in self.tasks.drain() {
            for handle in handles {
                handle.abort();
            }
        }
        // Stop all conference bridge handles (cancels their tasks)
        for (_, handle) in self.conference_bridge_handles.drain() {
            handle.stop();
        }
    }
}
