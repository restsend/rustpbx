use super::common::SessionData;
use super::config::{ActionNode, EntryAction};
use super::provider::{
    ActionProvider, ProviderContext, ProviderEvent, SessionContext, SessionEndReason,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThirdPartyNode {
    #[serde(default)]
    pub nodename: String,
    #[serde(default)]
    pub nodetype: String,
    #[serde(default)]
    pub nodevalue: String,
    #[serde(default)]
    pub businessnodeid: String,
    #[serde(default)]
    pub children: HashMap<String, String>,
    #[serde(default)]
    pub controltype: String,
}

#[derive(Debug, Clone)]
pub struct ThirdPartyTree {
    pub nodes: HashMap<String, ThirdPartyNode>,
    pub entry_id: String,
}

impl ThirdPartyTree {
    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        let nodes: HashMap<String, ThirdPartyNode> =
            serde_json::from_str(json).map_err(|e| anyhow::anyhow!("parse tree json: {}", e))?;
        let entry_id = nodes
            .iter()
            .find(|(_, n)| n.nodetype == "entry")
            .map(|(id, _)| id.clone())
            .ok_or_else(|| anyhow::anyhow!("no entry node found"))?;
        Ok(Self { nodes, entry_id })
    }

    pub fn merge_nodes(&mut self, new_nodes: HashMap<String, ThirdPartyNode>) {
        let count = new_nodes.len();
        self.nodes.extend(new_nodes);
        debug!(count, "merged dynamic tree nodes");
    }
}

#[derive(Debug, Clone, Default)]
struct ProviderState {
    current_node_id: Option<String>,
    awaiting_dtmf_menu_children: Option<HashMap<String, String>>,
    step_index: u32,
}

pub struct ThirdPartyTreeProvider {
    tree: Mutex<ThirdPartyTree>,
    state: Mutex<ProviderState>,
    base_api_url: String,
    sess: Mutex<SessionData>,
}

impl ThirdPartyTreeProvider {
    pub fn new(tree: ThirdPartyTree, base_api_url: String) -> Self {
        Self {
            tree: Mutex::new(tree),
            state: Mutex::new(ProviderState::default()),
            base_api_url,
            sess: Mutex::new(SessionData::default()),
        }
    }

    pub fn from_json(json: &str, base_api_url: String) -> anyhow::Result<Self> {
        let tree = ThirdPartyTree::from_json(json)?;
        Ok(Self::new(tree, base_api_url))
    }

    fn convert_node(&self, node: &ThirdPartyNode) -> EntryAction {
        match node.nodetype.as_str() {
            "api" => EntryAction::Api {
                url: node.nodename.clone(),
                method: Some("GET".into()),
                headers: HashMap::new(),
                variables: None,
                timeout: 10,
                get_dynamic_tree: if node.controltype == "getDynamicTree" {
                    Some(true)
                } else {
                    None
                },
            },

            "menu" => {
                let timeout: u64 = node.nodevalue.parse().unwrap_or(5);
                EntryAction::DtmfMenu {
                    greeting: Some(node.nodename.clone()).filter(|g| !g.is_empty()),
                    greeting_text: None,
                    greeting_record_list: None,
                    greeting_voice: None,
                    timeout_ms: timeout * 1000,
                    max_retries: 3,
                    entries: HashMap::new(),
                    timeout_action: None,
                    invalid_action: None,
                    greeting_api_url: None,
                }
            }

            "menu_tts" => {
                let timeout: u64 = node.nodevalue.parse().unwrap_or(5);
                EntryAction::DtmfMenu {
                    greeting: None,
                    greeting_text: Some(node.nodename.clone()).filter(|t| !t.is_empty()),
                    greeting_record_list: None,
                    greeting_voice: None,
                    timeout_ms: timeout * 1000,
                    max_retries: 3,
                    entries: HashMap::new(),
                    timeout_action: None,
                    invalid_action: None,
                    greeting_api_url: None,
                }
            }

            "menu_tts_api" => {
                let timeout: u64 = node.nodevalue.parse().unwrap_or(5);
                let api_url = self.build_node_api_url(&node.businessnodeid);
                EntryAction::DtmfMenu {
                    greeting: None,
                    greeting_text: None,
                    greeting_record_list: None,
                    greeting_voice: None,
                    timeout_ms: timeout * 1000,
                    max_retries: 3,
                    entries: HashMap::new(),
                    timeout_action: None,
                    invalid_action: None,
                    greeting_api_url: Some(api_url),
                }
            }

            "prompt" => EntryAction::Prompt {
                file: Some(node.nodename.clone()).filter(|f| !f.is_empty()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: false,
                tts_api_url: None,
            },

            "prompt_break" => EntryAction::Prompt {
                file: Some(node.nodename.clone()).filter(|f| !f.is_empty()),
                tts_text: None,
                tts_voice: None,
                record_name_list: None,
                interruptible: true,
                tts_api_url: None,
            },

            "prompt_tts_break_api" => {
                let api_url = self.build_node_api_url(&node.businessnodeid);
                EntryAction::Prompt {
                    file: None,
                    tts_text: None,
                    tts_voice: None,
                    record_name_list: None,
                    interruptible: true,
                    tts_api_url: Some(api_url),
                }
            }

            "toagent_by_kfb" => {
                let (skill_group_id, key_id) = Self::parse_toagent_nodevalue(&node.nodevalue);
                EntryAction::RouteToAgent {
                    target: node.nodename.clone(),
                    skill_group_id,
                    key_id,
                    channel_code: None,
                }
            }

            "toivr" => EntryAction::JumpIvr {
                route_point: node.nodename.clone(),
                params: HashMap::new(),
            },

            "syshangup" => EntryAction::Hangup {
                prompt: None,
                prompt_text: None,
                prompt_voice: None,
            },

            "input_phone" => EntryAction::InputPhone {
                prompt: Some(node.nodename.clone()).filter(|p| !p.is_empty()),
                prompt_text: None,
                prompt_voice: None,
                min_digits: 11,
                max_digits: 11,
                timeout_ms: 10_000,
                inter_digit_timeout_ms: 3_000,
                terminator: "#".into(),
            },

            "input_voice" => EntryAction::InputVoice {
                scene: node.nodename.clone(),
                timeout_ms: node.nodevalue.parse().unwrap_or(5000),
            },

            _ => {
                warn!(nodetype = %node.nodetype, "unknown third-party node type, treating as hangup");
                EntryAction::Hangup {
                    prompt: None,
                    prompt_text: None,
                    prompt_voice: None,
                }
            }
        }
    }

    fn build_node_api_url(&self, business_node_id: &str) -> String {
        let sess = self.sess.lock();
        let phone = sess.variables.get("caller").cloned().unwrap_or_default();
        let session_id = sess
            .variables
            .get("session_id")
            .cloned()
            .unwrap_or_default();
        format!(
            "{}?nodeid={}&business_node_id={}&phone={}&call_id={}",
            self.base_api_url, business_node_id, business_node_id, phone, session_id
        )
    }

    fn parse_toagent_nodevalue(nodevalue: &str) -> (Option<String>, Option<String>) {
        if nodevalue.is_empty() {
            return (None, None);
        }
        let parts: Vec<&str> = nodevalue.split(':').collect();
        if parts.len() >= 3 {
            (Some(parts[1].to_string()), Some(parts[2].to_string()))
        } else if parts.len() == 2 {
            (Some(parts[1].to_string()), None)
        } else {
            (None, None)
        }
    }

    fn get_next_linear_child(node: &ThirdPartyNode) -> Option<String> {
        node.children
            .get("0")
            .cloned()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                node.children
                    .values()
                    .next()
                    .cloned()
                    .filter(|s| !s.is_empty())
            })
    }

    fn build_linear_chain(tree: &ThirdPartyTree, node: &ThirdPartyNode) -> ActionNode {
        let action = convert_node_static(node);
        let next_id = Self::get_next_linear_child(node);
        let next = next_id.and_then(|id| {
            tree.nodes.get(&id).and_then(|n| {
                // Stop before menu/terminal nodes so they are fetched via
                // AudioComplete / Dtmf. Chaining a menu as `next` would skip
                // `awaiting_dtmf_menu_children` and treat DTMF key "0" as a
                // linear successor.
                if Self::is_menu_nodetype(&n.nodetype) || Self::is_terminal_nodetype(&n.nodetype) {
                    None
                } else {
                    Some(Box::new(Self::build_linear_chain(tree, n)))
                }
            })
        });
        ActionNode {
            action,
            wait_for_result: false,
            next,
            step_id: None,
            step_name: None,
            extra: None,
        }
    }

    fn is_terminal_nodetype(nodetype: &str) -> bool {
        matches!(nodetype, "syshangup" | "toagent_by_kfb" | "toivr")
    }

    fn is_menu_nodetype(nodetype: &str) -> bool {
        matches!(nodetype, "menu" | "menu_tts" | "menu_tts_api")
    }

    /// Convert a tree node for step execution. Menus are always provider-driven
    /// (empty `entries`) so DTMF is pushed via `next_action(Dtmf)`.
    fn action_for_node(
        &self,
        tree: &ThirdPartyTree,
        node: &ThirdPartyNode,
        state: &mut ProviderState,
    ) -> ActionNode {
        if Self::is_menu_nodetype(&node.nodetype) {
            state.awaiting_dtmf_menu_children = Some(node.children.clone());
            ActionNode::new(self.convert_node(node))
        } else if Self::is_terminal_nodetype(&node.nodetype) {
            ActionNode::new(self.convert_node(node))
        } else {
            Self::build_linear_chain(tree, node)
        }
    }

    fn replay_current_menu(&self, state: &mut ProviderState) -> ActionNode {
        let tree = self.tree.lock();
        if let Some(id) = state.current_node_id.as_ref() {
            if let Some(node) = tree.nodes.get(id) {
                if Self::is_menu_nodetype(&node.nodetype) {
                    state.awaiting_dtmf_menu_children = Some(node.children.clone());
                    return ActionNode::new(self.convert_node(node));
                }
            }
        }
        ActionNode::new(EntryAction::Hangup {
            prompt: None,
            prompt_text: None,
            prompt_voice: None,
        })
    }

    fn resolve_branch_key(body: &serde_json::Value) -> String {
        body.as_str()
            .map(|s| s.to_string())
            .or_else(|| {
                body.get("result")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                body.get("status")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                body.get("code")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .or_else(|| {
                body.get("code")
                    .and_then(|v| v.as_u64())
                    .map(|n| n.to_string())
            })
            .unwrap_or_else(|| "0".to_string())
    }
}

fn convert_node_static(node: &ThirdPartyNode) -> EntryAction {
    match node.nodetype.as_str() {
        "api" => EntryAction::Api {
            url: node.nodename.clone(),
            method: Some("GET".into()),
            headers: HashMap::new(),
            variables: None,
            timeout: 10,
            get_dynamic_tree: if node.controltype == "getDynamicTree" {
                Some(true)
            } else {
                None
            },
        },
        "menu" => {
            let timeout: u64 = node.nodevalue.parse().unwrap_or(5);
            EntryAction::DtmfMenu {
                greeting: Some(node.nodename.clone()).filter(|g| !g.is_empty()),
                greeting_text: None,
                greeting_record_list: None,
                greeting_voice: None,
                timeout_ms: timeout * 1000,
                max_retries: 3,
                entries: HashMap::new(),
                timeout_action: None,
                invalid_action: None,
                greeting_api_url: None,
            }
        }
        "menu_tts" => {
            let timeout: u64 = node.nodevalue.parse().unwrap_or(5);
            EntryAction::DtmfMenu {
                greeting: None,
                greeting_text: Some(node.nodename.clone()).filter(|t| !t.is_empty()),
                greeting_record_list: None,
                greeting_voice: None,
                timeout_ms: timeout * 1000,
                max_retries: 3,
                entries: HashMap::new(),
                timeout_action: None,
                invalid_action: None,
                greeting_api_url: None,
            }
        }
        "menu_tts_api" => EntryAction::DtmfMenu {
            greeting: None,
            greeting_text: None,
            greeting_record_list: None,
            greeting_voice: None,
            timeout_ms: node.nodevalue.parse().unwrap_or(5) * 1000,
            max_retries: 3,
            entries: HashMap::new(),
            timeout_action: None,
            invalid_action: None,
            greeting_api_url: Some(format!("placeholder://api?nodeid={}", node.businessnodeid)),
        },
        "prompt" => EntryAction::Prompt {
            file: Some(node.nodename.clone()).filter(|f| !f.is_empty()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: false,
            tts_api_url: None,
        },
        "prompt_break" => EntryAction::Prompt {
            file: Some(node.nodename.clone()).filter(|f| !f.is_empty()),
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: true,
            tts_api_url: None,
        },
        "prompt_tts_break_api" => EntryAction::Prompt {
            file: None,
            tts_text: None,
            tts_voice: None,
            record_name_list: None,
            interruptible: true,
            tts_api_url: Some(format!("placeholder://api?nodeid={}", node.businessnodeid)),
        },
        "toagent_by_kfb" => {
            let (skill_group_id, key_id) =
                ThirdPartyTreeProvider::parse_toagent_nodevalue(&node.nodevalue);
            EntryAction::RouteToAgent {
                target: node.nodename.clone(),
                skill_group_id,
                key_id,
                channel_code: None,
            }
        }
        "toivr" => EntryAction::JumpIvr {
            route_point: node.nodename.clone(),
            params: HashMap::new(),
        },
        "syshangup" => EntryAction::Hangup {
            prompt: None,
            prompt_text: None,
            prompt_voice: None,
        },
        "input_phone" => EntryAction::InputPhone {
            prompt: Some(node.nodename.clone()).filter(|p| !p.is_empty()),
            prompt_text: None,
            prompt_voice: None,
            min_digits: 11,
            max_digits: 11,
            timeout_ms: 10_000,
            inter_digit_timeout_ms: 3_000,
            terminator: "#".into(),
        },
        "input_voice" => EntryAction::InputVoice {
            scene: node.nodename.clone(),
            timeout_ms: node.nodevalue.parse().unwrap_or(5000),
        },
        _ => EntryAction::Hangup {
            prompt: None,
            prompt_text: None,
            prompt_voice: None,
        },
    }
}

#[async_trait]
impl ActionProvider for ThirdPartyTreeProvider {
    fn name(&self) -> &str {
        "third_party_tree"
    }

    async fn next_action(&self, ctx: ProviderContext) -> anyhow::Result<ActionNode> {
        let mut state = self.state.lock();
        state.step_index += 1;

        match ctx.event {
            Some(ProviderEvent::SessionStart) => {
                let tree = self.tree.lock();
                let entry = tree
                    .nodes
                    .get(&tree.entry_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("entry node {} not found", tree.entry_id))?;
                let first_child_id = entry.children.get("0").cloned().filter(|s| !s.is_empty());
                drop(tree);

                let node_id = first_child_id
                    .clone()
                    .unwrap_or_else(|| self.tree.lock().entry_id.clone());
                let node = {
                    let tree = self.tree.lock();
                    tree.nodes.get(&node_id).cloned()
                };
                let node = node.ok_or_else(|| anyhow::anyhow!("node {} not found", node_id))?;

                state.current_node_id = Some(node_id.clone());
                info!(
                    first_node = %node_id,
                    nodetype = %node.nodetype,
                    "ThirdPartyTree: starting tree traversal"
                );

                let tree = self.tree.lock();
                Ok(self.action_for_node(&tree, &node, &mut state))
            }

            Some(ProviderEvent::Dtmf { ref digit }) => {
                let menu_children = state.awaiting_dtmf_menu_children.take();

                if let Some(children) = menu_children {
                    let child_id = children
                        .get(digit.as_str())
                        .cloned()
                        .filter(|s| !s.is_empty());

                    if let Some(ref cid) = child_id {
                        state.current_node_id = Some(cid.clone());
                        let tree = self.tree.lock();
                        let child = tree.nodes.get(cid).cloned();

                        if let Some(child) = child {
                            info!(
                                node = %cid,
                                nodetype = %child.nodetype,
                                digit = %digit,
                                "ThirdPartyTree: DTMF matched"
                            );
                            return Ok(self.action_for_node(&tree, &child, &mut state));
                        }
                    }

                    // Unmatched key: restore the menu and re-offer it. Repeat
                    // would crash the step executor.
                    state.awaiting_dtmf_menu_children = Some(children);
                    info!(
                        digit = %digit,
                        "ThirdPartyTree: unmatched DTMF, replaying menu"
                    );
                    return Ok(self.replay_current_menu(&mut state));
                }

                Ok(self.replay_current_menu(&mut state))
            }

            Some(ProviderEvent::ApiResponse { ref body, .. }) => {
                let tree = self.tree.lock();
                let current_id = state.current_node_id.clone();
                let current = current_id
                    .as_ref()
                    .and_then(|id| tree.nodes.get(id))
                    .cloned();

                let Some(current) = current else {
                    return Ok(ActionNode::new(EntryAction::Hangup {
                        prompt: None,
                        prompt_text: None,
                        prompt_voice: None,
                    }));
                };

                if current.controltype == "getDynamicTree" {
                    if let Some(obj) = body.as_object() {
                        let parsed: HashMap<String, ThirdPartyNode> = obj
                            .iter()
                            .filter_map(|(k, v)| {
                                serde_json::from_value(v.clone())
                                    .ok()
                                    .map(|n: ThirdPartyNode| (k.clone(), n))
                            })
                            .collect();
                        if !parsed.is_empty() {
                            drop(tree);
                            {
                                let mut t = self.tree.lock();
                                t.merge_nodes(parsed);
                            }
                            let tree = self.tree.lock();
                            let next_id =
                                current.children.get("0").cloned().filter(|s| !s.is_empty());
                            if let Some(nid) = next_id {
                                if let Some(next) = tree.nodes.get(&nid).cloned() {
                                    state.current_node_id = Some(nid.clone());
                                    return Ok(self.action_for_node(&tree, &next, &mut state));
                                }
                            }
                            return Ok(ActionNode::new(EntryAction::Hangup {
                                prompt: None,
                                prompt_text: None,
                                prompt_voice: None,
                            }));
                        }
                    }
                }

                let branch_key = Self::resolve_branch_key(body);
                let child_id = current
                    .children
                    .get(&branch_key)
                    .cloned()
                    .filter(|s| !s.is_empty())
                    .or_else(|| current.children.get("0").cloned().filter(|s| !s.is_empty()));

                if let Some(cid) = child_id {
                    state.current_node_id = Some(cid.clone());
                    if let Some(child) = tree.nodes.get(&cid).cloned() {
                        info!(
                            node = %cid,
                            nodetype = %child.nodetype,
                            branch = %branch_key,
                            "ThirdPartyTree: API response branch"
                        );
                        return Ok(self.action_for_node(&tree, &child, &mut state));
                    }
                }

                Ok(ActionNode::new(EntryAction::Hangup {
                    prompt: None,
                    prompt_text: None,
                    prompt_voice: None,
                }))
            }

            Some(ProviderEvent::DtmfTimeout) | Some(ProviderEvent::DtmfMenuTimeout) => {
                Ok(self.replay_current_menu(&mut state))
            }

            Some(ProviderEvent::AudioComplete { .. }) => {
                let tree = self.tree.lock();
                let current_id = state.current_node_id.clone();
                let Some(cid) = current_id else {
                    return Ok(ActionNode::new(EntryAction::Hangup {
                        prompt: None,
                        prompt_text: None,
                        prompt_voice: None,
                    }));
                };
                let node = tree.nodes.get(&cid).cloned();
                let Some(node) = node else {
                    return Ok(ActionNode::new(EntryAction::Hangup {
                        prompt: None,
                        prompt_text: None,
                        prompt_voice: None,
                    }));
                };

                let next_id = Self::get_next_linear_child(&node);
                let Some(nid) = next_id else {
                    return Ok(ActionNode::new(EntryAction::Hangup {
                        prompt: None,
                        prompt_text: None,
                        prompt_voice: None,
                    }));
                };
                let next = tree.nodes.get(&nid).cloned();
                let Some(next) = next else {
                    return Ok(ActionNode::new(EntryAction::Hangup {
                        prompt: None,
                        prompt_text: None,
                        prompt_voice: None,
                    }));
                };

                state.current_node_id = Some(nid.clone());

                Ok(self.action_for_node(&tree, &next, &mut state))
            }

            _ => Ok(ActionNode::new(EntryAction::Repeat)),
        }
    }

    async fn on_session_start(&self, ctx: &SessionContext) -> anyhow::Result<()> {
        let mut sess = self.sess.lock();
        sess.variables
            .insert("session_id".into(), ctx.session_id.clone());
        sess.variables.insert("caller".into(), ctx.caller.clone());
        sess.variables.insert("callee".into(), ctx.callee.clone());
        sess.variables
            .insert("direction".into(), ctx.direction.clone());
        if let Some(ref tid) = ctx.tenant_id {
            sess.variables.insert("tenant_id".into(), tid.clone());
        }
        if let Some(ref iid) = ctx.ivr_id {
            sess.variables.insert("ivr_id".into(), iid.clone());
        }
        Ok(())
    }

    async fn on_session_end(
        &self,
        _reason: &SessionEndReason,
        _session_id: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree_json() -> String {
        r#"{
            "entry_1": {
                "nodename": "",
                "nodetype": "entry",
                "nodevalue": "",
                "businessnodeid": "710000",
                "children": {"0": "api_1"},
                "controltype": ""
            },
            "api_1": {
                "nodename": "http://example.com/api/check",
                "nodetype": "api",
                "nodevalue": "",
                "businessnodeid": "200001",
                "children": {"0": "prompt_1", "1": "menu_1"},
                "controltype": ""
            },
            "prompt_1": {
                "nodename": "welcome.wav",
                "nodetype": "prompt_break",
                "nodevalue": "",
                "businessnodeid": "200002",
                "children": {"0": "menu_1"},
                "controltype": ""
            },
            "menu_1": {
                "nodename": "主菜单请按1，人工请按0",
                "nodetype": "menu_tts",
                "nodevalue": "3",
                "businessnodeid": "200003",
                "children": {"1": "agent_1", "0": "ivr_1"},
                "controltype": ""
            },
            "agent_1": {
                "nodename": "39257",
                "nodetype": "toagent_by_kfb",
                "nodevalue": "from_gate:2618",
                "businessnodeid": "200004",
                "children": {"0": ""},
                "controltype": ""
            },
            "ivr_1": {
                "nodename": "39299",
                "nodetype": "toivr",
                "nodevalue": "",
                "businessnodeid": "200005",
                "children": {"0": ""},
                "controltype": ""
            },
            "hangup_1": {
                "nodename": "",
                "nodetype": "syshangup",
                "nodevalue": "",
                "businessnodeid": "200006",
                "children": {"0": ""},
                "controltype": ""
            }
        }"#
        .to_string()
    }

    #[test]
    fn test_parse_tree() {
        let tree = ThirdPartyTree::from_json(&sample_tree_json()).unwrap();
        assert_eq!(tree.entry_id, "entry_1");
        assert_eq!(tree.nodes.len(), 7);
    }

    #[test]
    fn test_convert_api_node() {
        let tree = ThirdPartyTree::from_json(&sample_tree_json()).unwrap();
        let provider = ThirdPartyTreeProvider::new(tree, "http://localhost".into());
        let tree = provider.tree.lock();
        let api_node = tree.nodes.get("api_1").unwrap();
        let action = provider.convert_node(api_node);
        match action {
            EntryAction::Api { url, .. } => {
                assert_eq!(url, "http://example.com/api/check");
            }
            _ => panic!("expected Api"),
        }
    }

    #[test]
    fn test_convert_menu_tts_node() {
        let tree = ThirdPartyTree::from_json(&sample_tree_json()).unwrap();
        let provider = ThirdPartyTreeProvider::new(tree, "http://localhost".into());
        let tree = provider.tree.lock();
        let menu_node = tree.nodes.get("menu_1").unwrap();
        let action = provider.convert_node(menu_node);
        match action {
            EntryAction::DtmfMenu {
                greeting_text,
                timeout_ms,
                entries,
                ..
            } => {
                assert_eq!(greeting_text.as_deref(), Some("主菜单请按1，人工请按0"));
                assert_eq!(timeout_ms, 3000);
                assert!(
                    entries.is_empty(),
                    "menu_tts must convert to provider-driven empty entries"
                );
            }
            _ => panic!("expected DtmfMenu"),
        }
    }

    #[test]
    fn test_convert_toagent_node() {
        let tree = ThirdPartyTree::from_json(&sample_tree_json()).unwrap();
        let provider = ThirdPartyTreeProvider::new(tree, "http://localhost".into());
        let tree = provider.tree.lock();
        let agent_node = tree.nodes.get("agent_1").unwrap();
        let action = provider.convert_node(agent_node);
        match action {
            EntryAction::RouteToAgent {
                target,
                skill_group_id,
                ..
            } => {
                assert_eq!(target, "39257");
                assert_eq!(skill_group_id.as_deref(), Some("2618"));
            }
            _ => panic!("expected RouteToAgent"),
        }
    }

    #[test]
    fn test_parse_toagent_nodevalue() {
        assert_eq!(
            ThirdPartyTreeProvider::parse_toagent_nodevalue("from_gate:2618"),
            (Some("2618".into()), None)
        );
        assert_eq!(
            ThirdPartyTreeProvider::parse_toagent_nodevalue("from_gate:4933:extra"),
            (Some("4933".into()), Some("extra".into()))
        );
        assert_eq!(
            ThirdPartyTreeProvider::parse_toagent_nodevalue("from_gate"),
            (None, None)
        );
        assert_eq!(
            ThirdPartyTreeProvider::parse_toagent_nodevalue(""),
            (None, None)
        );
    }

    #[test]
    fn test_build_linear_chain() {
        let tree = ThirdPartyTree::from_json(&sample_tree_json()).unwrap();
        let node = tree.nodes.get("prompt_1").unwrap().clone();
        let chain = ThirdPartyTreeProvider::build_linear_chain(&tree, &node);
        match &chain.action {
            EntryAction::Prompt {
                file,
                interruptible,
                ..
            } => {
                assert_eq!(file.as_deref(), Some("welcome.wav"));
                assert!(*interruptible);
            }
            _ => panic!("expected Prompt"),
        }
        assert!(
            chain.next.is_none(),
            "linear chain must stop before menu_tts so DTMF is provider-driven"
        );
    }

    fn test_provider_ctx(event: ProviderEvent) -> ProviderContext {
        ProviderContext {
            session_id: "s1".into(),
            app_execution_id: 1,
            caller: "1001".into(),
            callee: "2000".into(),
            direction: "inbound".into(),
            tenant_id: None,
            ivr_id: None,
            variables: HashMap::new(),
            sip_headers: None,
            event: Some(event),
            route_name: None,
            custom_data: None,
            step_start_time: None,
            step_end_time: None,
            step_duration_ms: None,
            step_index: None,
            transferred_from: None,
        }
    }

    async fn drive_to_menu_tts(provider: &ThirdPartyTreeProvider) -> ActionNode {
        let start = provider
            .next_action(test_provider_ctx(ProviderEvent::SessionStart))
            .await
            .unwrap();
        assert!(
            matches!(start.action, EntryAction::Api { .. }),
            "entry child should be api"
        );

        let after_api = provider
            .next_action(test_provider_ctx(ProviderEvent::ApiResponse {
                status: 200,
                body: serde_json::json!("0"),
            }))
            .await
            .unwrap();
        assert!(
            matches!(after_api.action, EntryAction::Prompt { .. }),
            "api branch 0 should be prompt_break"
        );
        assert!(after_api.next.is_none(), "prompt must not chain menu_tts");

        let menu = provider
            .next_action(test_provider_ctx(ProviderEvent::AudioComplete {
                interrupted: false,
            }))
            .await
            .unwrap();
        match &menu.action {
            EntryAction::DtmfMenu {
                greeting_text,
                entries,
                ..
            } => {
                assert_eq!(greeting_text.as_deref(), Some("主菜单请按1，人工请按0"));
                assert!(
                    entries.is_empty(),
                    "menu_tts must be provider-driven (empty entries)"
                );
            }
            other => panic!("expected DtmfMenu, got {other:?}"),
        }
        menu
    }

    #[tokio::test]
    async fn test_menu_tts_dtmf_matched_pushes_digit() {
        let provider =
            ThirdPartyTreeProvider::from_json(&sample_tree_json(), "http://localhost".into())
                .unwrap();
        drive_to_menu_tts(&provider).await;

        let next = provider
            .next_action(test_provider_ctx(ProviderEvent::Dtmf { digit: "1".into() }))
            .await
            .unwrap();
        match next.action {
            EntryAction::RouteToAgent { ref target, .. } => {
                assert_eq!(target, "39257");
            }
            other => panic!("expected RouteToAgent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_menu_tts_unmatched_dtmf_replays_menu() {
        let provider =
            ThirdPartyTreeProvider::from_json(&sample_tree_json(), "http://localhost".into())
                .unwrap();
        drive_to_menu_tts(&provider).await;

        let replayed = provider
            .next_action(test_provider_ctx(ProviderEvent::Dtmf { digit: "9".into() }))
            .await
            .unwrap();
        match &replayed.action {
            EntryAction::DtmfMenu {
                greeting_text,
                entries,
                ..
            } => {
                assert_eq!(greeting_text.as_deref(), Some("主菜单请按1，人工请按0"));
                assert!(entries.is_empty());
            }
            other => panic!("expected replayed DtmfMenu, got {other:?}"),
        }

        let matched = provider
            .next_action(test_provider_ctx(ProviderEvent::Dtmf { digit: "1".into() }))
            .await
            .unwrap();
        assert!(
            matches!(matched.action, EntryAction::RouteToAgent { .. }),
            "menu must still accept a valid key after unmatched DTMF"
        );
    }

    #[tokio::test]
    async fn test_menu_tts_timeout_replays_menu() {
        let provider =
            ThirdPartyTreeProvider::from_json(&sample_tree_json(), "http://localhost".into())
                .unwrap();
        drive_to_menu_tts(&provider).await;

        let replayed = provider
            .next_action(test_provider_ctx(ProviderEvent::DtmfMenuTimeout))
            .await
            .unwrap();
        match replayed.action {
            EntryAction::DtmfMenu { greeting_text, .. } => {
                assert_eq!(greeting_text.as_deref(), Some("主菜单请按1，人工请按0"));
            }
            other => panic!("expected replayed DtmfMenu, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_menu_tts_digit_zero_jumps_ivr() {
        let provider =
            ThirdPartyTreeProvider::from_json(&sample_tree_json(), "http://localhost".into())
                .unwrap();
        drive_to_menu_tts(&provider).await;

        let next = provider
            .next_action(test_provider_ctx(ProviderEvent::Dtmf { digit: "0".into() }))
            .await
            .unwrap();
        match next.action {
            EntryAction::JumpIvr {
                ref route_point, ..
            } => {
                assert_eq!(route_point, "39299");
            }
            other => panic!("DTMF 0 must be a menu key to toivr, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_menu_tts_dtmf_timeout_replays_menu() {
        let provider =
            ThirdPartyTreeProvider::from_json(&sample_tree_json(), "http://localhost".into())
                .unwrap();
        drive_to_menu_tts(&provider).await;

        let replayed = provider
            .next_action(test_provider_ctx(ProviderEvent::DtmfTimeout))
            .await
            .unwrap();
        assert!(
            matches!(replayed.action, EntryAction::DtmfMenu { .. }),
            "DtmfTimeout must replay menu_tts, not Repeat"
        );
    }

    #[tokio::test]
    async fn test_session_start_on_menu_tts_is_provider_driven() {
        let json = r#"{
            "entry_1": {
                "nodename": "",
                "nodetype": "entry",
                "nodevalue": "",
                "businessnodeid": "1",
                "children": {"0": "menu_1"},
                "controltype": ""
            },
            "menu_1": {
                "nodename": "请按1",
                "nodetype": "menu_tts",
                "nodevalue": "5",
                "businessnodeid": "2",
                "children": {"1": "hangup_1"},
                "controltype": ""
            },
            "hangup_1": {
                "nodename": "",
                "nodetype": "syshangup",
                "nodevalue": "",
                "businessnodeid": "3",
                "children": {"0": ""},
                "controltype": ""
            }
        }"#;
        let provider = ThirdPartyTreeProvider::from_json(json, "http://localhost".into()).unwrap();
        let menu = provider
            .next_action(test_provider_ctx(ProviderEvent::SessionStart))
            .await
            .unwrap();
        match &menu.action {
            EntryAction::DtmfMenu {
                greeting_text,
                entries,
                ..
            } => {
                assert_eq!(greeting_text.as_deref(), Some("请按1"));
                assert!(entries.is_empty());
            }
            other => panic!("expected menu_tts DtmfMenu, got {other:?}"),
        }

        let hangup = provider
            .next_action(test_provider_ctx(ProviderEvent::Dtmf { digit: "1".into() }))
            .await
            .unwrap();
        assert!(
            matches!(hangup.action, EntryAction::Hangup { .. }),
            "matched DTMF must execute the child node"
        );
    }
}
