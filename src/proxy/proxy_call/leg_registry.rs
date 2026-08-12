use crate::call::domain::{Leg, LegId};
use crate::call::runtime::conference_media_bridge::ConferenceBridgeHandle;
use crate::proxy::proxy_call::media_peer::MediaPeer;
use rsipstack::dialog::dialog::Dialog;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinHandle;

/// All per-leg data bundled into a single struct so add/remove cannot forget
/// to touch one of the (former) parallel maps.
struct LegData {
    leg: Leg,
    dialog: Option<Dialog>,
    /// `None` when the leg exists (via `insert`) but no peer has been set yet.
    peer: Option<Arc<dyn MediaPeer>>,
    transport: Option<rustrtc::TransportMode>,
    answer: Option<String>,
    has_video: bool,
    tasks: Vec<JoinHandle<()>>,
    conference_bridge: Option<ConferenceBridgeHandle>,
}

pub struct LegRegistry {
    legs: HashMap<LegId, LegData>,
}

impl LegRegistry {
    pub fn new() -> Self {
        Self {
            legs: HashMap::new(),
        }
    }

    pub fn add_leg(
        &mut self,
        id: LegId,
        state: Leg,
        peer: Arc<dyn MediaPeer>,
        dialog: Option<Dialog>,
    ) {
        self.legs.insert(
            id,
            LegData {
                leg: state,
                dialog,
                peer: Some(peer),
                transport: None,
                answer: None,
                has_video: false,
                tasks: Vec::new(),
                conference_bridge: None,
            },
        );
    }

    pub fn remove(&mut self, id: &LegId) -> Option<Leg> {
        let data = self.legs.remove(id)?;
        if let Some(handle) = data.conference_bridge {
            handle.stop();
        }
        for handle in data.tasks {
            handle.abort();
        }
        Some(data.leg)
    }

    pub fn set_dialog(&mut self, id: LegId, dialog: Dialog) {
        if let Some(data) = self.legs.get_mut(&id) {
            data.dialog = Some(dialog);
        }
    }

    pub fn get_dialog(&self, id: &LegId) -> Option<&Dialog> {
        self.legs.get(id).and_then(|d| d.dialog.as_ref())
    }

    pub fn retain_dialogs_by_dialog_id(&mut self, terminated_id: &rsipstack::dialog::DialogId) {
        for data in self.legs.values_mut() {
            if let Some(ref dlg) = data.dialog {
                if dlg.id() == *terminated_id {
                    data.dialog = None;
                }
            }
        }
    }

    pub fn get_peer(&self, id: &LegId) -> Option<&Arc<dyn MediaPeer>> {
        self.legs.get(id).and_then(|d| d.peer.as_ref())
    }

    pub fn set_peer(&mut self, id: LegId, peer: Arc<dyn MediaPeer>) {
        self.legs
            .entry(id)
            .or_insert_with(|| LegData {
                leg: Leg::new(LegId::new("")),
                dialog: None,
                peer: None,
                transport: None,
                answer: None,
                has_video: false,
                tasks: Vec::new(),
                conference_bridge: None,
            })
            .peer = Some(peer);
    }

    pub fn caller_peer(&self) -> Option<&Arc<dyn MediaPeer>> {
        self.get_peer(&LegId::new("caller"))
    }

    pub fn callee_peer(&self) -> Option<&Arc<dyn MediaPeer>> {
        self.get_peer(&LegId::new("callee"))
    }

    pub fn get_transport(&self, id: &LegId) -> Option<rustrtc::TransportMode> {
        self.legs.get(id).and_then(|d| d.transport.clone())
    }

    pub fn set_transport(&mut self, id: LegId, transport: rustrtc::TransportMode) {
        if let Some(data) = self.legs.get_mut(&id) {
            data.transport = Some(transport);
        }
    }

    pub fn caller_is_webrtc(&self) -> bool {
        self.get_transport(&LegId::new("caller"))
            .map(|t| t == rustrtc::TransportMode::WebRtc)
            .unwrap_or(false)
    }

    pub fn callee_is_webrtc(&self) -> bool {
        self.get_transport(&LegId::new("callee"))
            .map(|t| t == rustrtc::TransportMode::WebRtc)
            .unwrap_or(false)
    }

    pub fn get_answer(&self, id: &LegId) -> Option<&str> {
        self.legs.get(id).and_then(|d| d.answer.as_deref())
    }

    pub fn set_answer(&mut self, id: LegId, answer: String) {
        if let Some(data) = self.legs.get_mut(&id) {
            data.answer = Some(answer);
        }
    }

    pub fn leg_has_video(&self, id: &LegId) -> bool {
        self.legs.get(id).map(|d| d.has_video).unwrap_or(false)
    }

    pub fn set_video_state(&mut self, id: &LegId, has_video: bool) {
        if let Some(data) = self.legs.get_mut(id) {
            data.has_video = has_video;
        }
    }

    pub fn push_task(&mut self, id: LegId, handle: JoinHandle<()>) {
        if let Some(data) = self.legs.get_mut(&id) {
            data.tasks.push(handle);
        }
    }

    pub fn drain_tasks(&mut self) -> impl Iterator<Item = (LegId, Vec<JoinHandle<()>>)> + '_ {
        self.legs.iter_mut().filter_map(|(id, data)| {
            if data.tasks.is_empty() {
                None
            } else {
                Some((id.clone(), std::mem::take(&mut data.tasks)))
            }
        })
    }

    pub fn set_conference_bridge_handle(&mut self, id: LegId, handle: ConferenceBridgeHandle) {
        if let Some(data) = self.legs.get_mut(&id) {
            if let Some(old) = data.conference_bridge.take() {
                old.stop();
            }
            data.conference_bridge = Some(handle);
        }
    }

    pub fn remove_conference_bridge_handle(
        &mut self,
        id: &LegId,
    ) -> Option<ConferenceBridgeHandle> {
        self.legs.get_mut(id).and_then(|d| d.conference_bridge.take())
    }

    pub fn stop_all_conference_bridge_handles(&mut self) {
        for data in self.legs.values_mut() {
            if let Some(handle) = data.conference_bridge.take() {
                handle.stop();
            }
        }
    }

    pub fn contains_key(&self, id: &LegId) -> bool {
        self.legs.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.legs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.legs.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&LegId, &Leg)> {
        self.legs.iter().map(|(id, d)| (id, &d.leg))
    }

    pub fn get(&self, id: &LegId) -> Option<&Leg> {
        self.legs.get(id).map(|d| &d.leg)
    }

    pub fn get_mut(&mut self, id: &LegId) -> Option<&mut Leg> {
        self.legs.get_mut(id).map(|d| &mut d.leg)
    }

    pub fn values(&self) -> impl Iterator<Item = &Leg> {
        self.legs.values().map(|d| &d.leg)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Leg> {
        self.legs.values_mut().map(|d| &mut d.leg)
    }

    pub fn keys(&self) -> impl Iterator<Item = &LegId> {
        self.legs.keys()
    }

    pub fn insert(&mut self, id: LegId, state: Leg) {
        if let Some(data) = self.legs.get_mut(&id) {
            data.leg = state;
        } else {
            self.legs.insert(
                id,
                LegData {
                    leg: state,
                    dialog: None,
                    peer: None,
                    transport: None,
                    answer: None,
                    has_video: false,
                    tasks: Vec::new(),
                    conference_bridge: None,
                },
            );
        }
    }
}

impl Default for LegRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LegRegistry {
    fn drop(&mut self) {
        for data in self.legs.values() {
            for handle in &data.tasks {
                handle.abort();
            }
            if let Some(handle) = &data.conference_bridge {
                handle.stop();
            }
        }
    }
}
