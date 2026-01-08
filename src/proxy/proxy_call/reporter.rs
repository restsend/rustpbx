use crate::{
    call::{CalleeDisplayName, TransactionCookie, TrunkContext},
    callrecord::{
        CallRecord, CallRecordExtras, CallRecordHangupMessage, CallRecordHangupReason,
        CallRecordMedia, CallRecordSender, sipflow::SipMessageItem,
    },
    models::call_record::{CallRecordPersistArgs, extract_sip_username},
    proxy::{
        proxy_call::{session::CallSessionRecordSnapshot, state::CallContext},
        server::SipServerRef,
    },
};
use chrono::{Duration, Utc};
use rsip::prelude::HeadersExt;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
};

pub struct CallReporter {
    pub server: SipServerRef,
    pub context: CallContext,
    pub call_record_sender: Option<CallRecordSender>,
}

impl CallReporter {
    pub(super) fn report(&self, snapshot: CallSessionRecordSnapshot) {
        let now = Utc::now();
        let start_time =
            now - Duration::from_std(self.context.start_time.elapsed()).unwrap_or_default();

        let ring_time = snapshot.ring_time.map(|rt| {
            start_time
                + Duration::from_std(rt.duration_since(self.context.start_time)).unwrap_or_default()
        });

        let answer_time = snapshot.answer_time.map(|at| {
            start_time
                + Duration::from_std(at.duration_since(self.context.start_time)).unwrap_or_default()
        });

        let status_code = snapshot
            .last_error
            .as_ref()
            .map(|(code, _)| u16::from(code.clone()))
            .unwrap_or(200);

        let hangup_reason = snapshot.hangup_reason.clone().or_else(|| {
            if snapshot.last_error.is_some() {
                Some(CallRecordHangupReason::Failed)
            } else if snapshot.answer_time.is_some() {
                Some(CallRecordHangupReason::BySystem)
            } else {
                Some(CallRecordHangupReason::Failed)
            }
        });

        let original_caller = snapshot
            .original_caller
            .clone()
            .or_else(|| self.context.dialplan.caller.as_ref().map(|c| c.to_string()))
            .unwrap_or_default();

        let original_callee = snapshot
            .original_callee
            .clone()
            .or_else(|| {
                self.context
                    .dialplan
                    .original
                    .to_header()
                    .ok()
                    .and_then(|to_header| to_header.uri().ok().map(|uri| uri.to_string()))
            })
            .or_else(|| {
                self.context
                    .dialplan
                    .first_target()
                    .map(|location| location.aor.to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());

        let caller = snapshot
            .routed_caller
            .clone()
            .unwrap_or_else(|| original_caller.clone());

        let callee = snapshot
            .routed_callee
            .clone()
            .or_else(|| snapshot.connected_callee.clone())
            .unwrap_or_else(|| original_callee.clone());

        let mut extras_map: HashMap<String, Value> = HashMap::new();
        extras_map.insert(
            "status_code".to_string(),
            Value::Number(JsonNumber::from(status_code)),
        );
        if let Some(reason) = hangup_reason.as_ref() {
            extras_map.insert(
                "hangup_reason".to_string(),
                Value::String(reason.to_string()),
            );
        }
        if let Some((code, reason)) = snapshot.last_error.as_ref() {
            extras_map.insert(
                "last_error_code".to_string(),
                Value::Number(JsonNumber::from(u16::from(code.clone()))),
            );
            if let Some(reason) = reason {
                extras_map.insert(
                    "last_error_reason".to_string(),
                    Value::String(reason.clone()),
                );
            }
        }

        if let Some(queue) = snapshot.last_queue_name.clone() {
            extras_map.insert("last_queue".to_string(), Value::String(queue));
        }

        let mut hangup_messages = snapshot.hangup_messages.clone();
        if hangup_messages.is_empty() {
            if let Some((code, reason)) = snapshot.last_error.as_ref() {
                hangup_messages.push(CallRecordHangupMessage {
                    code: u16::from(code.clone()),
                    reason: reason.clone(),
                    target: None,
                });
            }
        }
        if !hangup_messages.is_empty() {
            if let Ok(value) = serde_json::to_value(&hangup_messages) {
                extras_map.insert("hangup_messages".to_string(), value);
            }
        }

        let mut rewrite_payload = JsonMap::new();
        rewrite_payload.insert(
            "caller_original".to_string(),
            Value::String(original_caller.clone()),
        );
        rewrite_payload.insert("caller_final".to_string(), Value::String(caller.clone()));
        rewrite_payload.insert(
            "callee_original".to_string(),
            Value::String(original_callee.clone()),
        );
        rewrite_payload.insert("callee_final".to_string(), Value::String(callee.clone()));
        if let Some(contact) = snapshot.routed_contact.as_ref() {
            rewrite_payload.insert("contact".to_string(), Value::String(contact.clone()));
        }
        if let Some(destination) = snapshot.routed_destination.as_ref() {
            rewrite_payload.insert(
                "destination".to_string(),
                Value::String(destination.clone()),
            );
        }
        extras_map.insert("rewrite".to_string(), Value::Object(rewrite_payload));

        let mut sip_flows_map: HashMap<String, Vec<SipMessageItem>> = HashMap::new();
        let server_dialog_id = snapshot.server_dialog_id.clone();
        let mut call_ids: HashSet<String> = HashSet::new();
        call_ids.insert(server_dialog_id.call_id.clone());

        for dialog_id in &snapshot.callee_dialogs {
            call_ids.insert(dialog_id.call_id.clone());
        }

        let mut sip_leg_roles: HashMap<String, String> = HashMap::new();
        sip_leg_roles.insert(
            crate::utils::sanitize_id(&server_dialog_id.call_id),
            "primary".to_string(),
        );
        for dialog_id in &snapshot.callee_dialogs {
            sip_leg_roles
                .entry(crate::utils::sanitize_id(&dialog_id.call_id))
                .or_insert_with(|| "b2bua".to_string());
        }

        // Only collect SIP flows if enabled in dialplan
        if self.context.dialplan.enable_sipflow {
            for call_id in &call_ids {
                if let Some(items) = self.server.drain_sip_flow(call_id) {
                    if !items.is_empty() {
                        sip_flows_map.insert(crate::utils::sanitize_id(call_id), items);
                    }
                }
            }
        }

        let direction = self.context.dialplan.direction.to_string();

        // Helper to resolve call status (copied from proxy_call.rs logic)
        let status = if snapshot.answer_time.is_some() {
            "completed".to_string()
        } else if snapshot.last_error.is_some() {
            "failed".to_string()
        } else {
            "missed".to_string()
        };

        let (from_number, from_name, department_id, extension_id) =
            resolve_user_info(&self.context.cookie, &caller);
        let to_number = extract_sip_username(&callee);
        let to_name = self
            .context
            .cookie
            .get_extension::<CalleeDisplayName>()
            .map(|e| e.0);
        let trunk_context = self.context.cookie.get_extension::<TrunkContext>();
        let (sip_gateway, sip_trunk_id) = if let Some(ctx) = trunk_context {
            (Some(ctx.name.clone()), ctx.id)
        } else {
            (None, None)
        };

        let mut recorder = Vec::new();
        if self.context.dialplan.recording.enabled {
            if let Some(recorder_config) = self.context.dialplan.recording.option.as_ref() {
                if !recorder_config.recorder_file.is_empty() {
                    let size = fs::metadata(&recorder_config.recorder_file)
                        .map(|meta| meta.len())
                        .unwrap_or(0);
                    recorder.push(CallRecordMedia {
                        track_id: "mixed".to_string(),
                        path: recorder_config.recorder_file.clone(),
                        size,
                        extra: None,
                    });
                }
            }
        }

        // Copy values from cookie to extras_map
        // (Removed as TransactionCookie no longer has values)

        let metadata_value = if extras_map.is_empty() {
            None
        } else {
            Some(serde_json::to_value(&extras_map).unwrap_or_default())
        };
        let recording_path_for_db = recorder.first().map(|media| media.path.clone());

        let mut persist_args = CallRecordPersistArgs::default();
        persist_args.direction = direction;
        persist_args.status = status;
        persist_args.from_number = from_number;
        persist_args.to_number = to_number;
        persist_args.caller_name = from_name;
        persist_args.agent_name = to_name;
        persist_args.queue = snapshot.last_queue_name.clone();
        persist_args.department_id = department_id;
        persist_args.extension_id = extension_id;
        persist_args.sip_trunk_id = sip_trunk_id;
        persist_args.sip_gateway = sip_gateway;
        persist_args.metadata = metadata_value;
        persist_args.recording_url = recording_path_for_db;

        let mut record = CallRecord {
            call_id: self.context.session_id.clone(),
            start_time,
            ring_time,
            answer_time,
            end_time: now,
            caller: caller.clone(),
            callee: callee.clone(),
            status_code,
            hangup_reason: hangup_reason.clone(),
            hangup_messages: hangup_messages.clone(),
            recorder,
            sip_flows: sip_flows_map,
            sip_leg_roles,
            extensions: snapshot.extensions,
        };

        record.extensions.insert(persist_args);
        record.extensions.insert(CallRecordExtras(extras_map));

        if let Some(ref sender) = self.call_record_sender {
            let _ = sender.send(record);
        }
    }
}

fn resolve_user_info(
    cookie: &TransactionCookie,
    caller_uri: &str,
) -> (Option<String>, Option<String>, Option<i64>, Option<i64>) {
    let mut from_number = extract_sip_username(caller_uri);
    let (from_display_name, department_id, extension_id) = if let Some(user) = cookie.get_user() {
        let mut dept_id = None;
        let mut is_wholesale = false;

        if let Some(deps) = &user.departments {
            for d in deps {
                if d.starts_with("tenant:") {
                    is_wholesale = true;
                } else if let Ok(id) = d.parse::<i64>() {
                    dept_id = Some(id);
                }
            }
        }

        if is_wholesale {
            from_number = Some(user.username.clone());
        }

        let ext_id = if user.id > 0 {
            Some(user.id as i64)
        } else {
            None
        };
        (user.display_name, dept_id, ext_id)
    } else {
        (None, None, None)
    };

    (from_number, from_display_name, department_id, extension_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::SipUser;

    #[test]
    fn test_resolve_user_info_wholesale() {
        let cookie = TransactionCookie::default();
        let mut user = SipUser::default();
        user.username = "1234".to_string();
        user.display_name = Some("alice".to_string());
        user.departments = Some(vec!["tenant:100".to_string()]);
        cookie.set_user(user);

        let caller = "sip:mock-uuid@1.2.3.4";
        let (from, from_name, dept, ext) = resolve_user_info(&cookie, caller);

        assert_eq!(from, Some("1234".to_string()));
        assert_eq!(from_name, Some("alice".to_string()));
        assert_eq!(dept, None);
        assert_eq!(ext, None);
    }

    #[test]
    fn test_resolve_user_info_mixed() {
        let cookie = TransactionCookie::default();
        let mut user = SipUser::default();
        user.username = "1234".to_string();
        user.display_name = Some("alice".to_string());
        user.departments = Some(vec!["tenant:100".to_string(), "5".to_string()]);
        user.id = 99;
        cookie.set_user(user);

        let caller = "sip:mock-uuid@1.2.3.4";
        let (from, from_name, dept, ext) = resolve_user_info(&cookie, caller);

        assert_eq!(from, Some("1234".to_string()));
        assert_eq!(from_name, Some("alice".to_string()));
        assert_eq!(dept, Some(5));
        assert_eq!(ext, Some(99));
    }

    #[test]
    fn test_resolve_user_info_normal() {
        let cookie = TransactionCookie::default();
        let mut user = SipUser::default();
        user.username = "1001".to_string();
        user.display_name = Some("alice".to_string());
        user.departments = Some(vec!["5".to_string()]);
        user.id = 99;
        cookie.set_user(user);

        let caller = "sip:1001@1.2.3.4";
        let (from, from_name, dept, ext) = resolve_user_info(&cookie, caller);

        assert_eq!(from, Some("1001".to_string()));
        assert_eq!(from_name, Some("alice".to_string()));
        assert_eq!(dept, Some(5));
        assert_eq!(ext, Some(99));
    }
}
