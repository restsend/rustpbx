use crate::{
    call::{Dialplan, TransactionCookie},
    callrecord::{
        CallRecord, CallRecordHangupMessage, CallRecordHangupReason, CallRecordMedia,
        CallRecordPersistArgs, CallRecordSender, apply_record_file_extras, extract_sip_username,
        extras_map_to_metadata, extras_map_to_option, sipflow::SipMessageItem,
    },
    proxy::{proxy_call::session::CallSessionRecordSnapshot, server::SipServerRef},
};
use chrono::{Duration, Utc};
use rsip::prelude::HeadersExt;
use serde_json::{Map as JsonMap, Number as JsonNumber, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::Arc,
    time::Instant,
};

pub struct CallReporter {
    pub server: SipServerRef,
    pub start_time: Instant,
    pub session_id: String,
    pub dialplan: Arc<Dialplan>,
    pub cookie: TransactionCookie,
    pub call_record_sender: Option<CallRecordSender>,
}

impl CallReporter {
    pub(super) fn report(&self, snapshot: CallSessionRecordSnapshot) {
        let now = Utc::now();
        let start_time = now - Duration::from_std(self.start_time.elapsed()).unwrap_or_default();

        let ring_time = snapshot.ring_time.map(|rt| {
            start_time + Duration::from_std(rt.duration_since(self.start_time)).unwrap_or_default()
        });

        let answer_time = snapshot.answer_time.map(|at| {
            start_time + Duration::from_std(at.duration_since(self.start_time)).unwrap_or_default()
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
            .or_else(|| self.dialplan.caller.as_ref().map(|c| c.to_string()))
            .unwrap_or_default();

        let original_callee = snapshot
            .original_callee
            .clone()
            .or_else(|| {
                self.dialplan
                    .original
                    .to_header()
                    .ok()
                    .and_then(|to_header| to_header.uri().ok().map(|uri| uri.to_string()))
            })
            .or_else(|| {
                self.dialplan
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

        if let Some(trace) = snapshot.ivr_trace.clone() {
            let mut ivr_payload = JsonMap::new();
            if let Some(reference) = trace.reference.clone() {
                ivr_payload.insert("reference".to_string(), Value::String(reference));
            }
            if let Some(plan_id) = trace.plan_id.clone() {
                ivr_payload.insert("plan_id".to_string(), Value::String(plan_id));
            }
            if let Some(exit) = trace.exit.clone() {
                ivr_payload.insert("exit".to_string(), Value::String(exit));
            }
            if let Some(detail) = trace.detail.clone() {
                ivr_payload.insert("detail".to_string(), Value::String(detail));
            }
            if !ivr_payload.is_empty() {
                extras_map.insert("ivr".to_string(), Value::Object(ivr_payload));
            }
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
        sip_leg_roles.insert(server_dialog_id.call_id.clone(), "primary".to_string());
        for dialog_id in &snapshot.callee_dialogs {
            sip_leg_roles
                .entry(dialog_id.call_id.clone())
                .or_insert_with(|| "b2bua".to_string());
        }

        for call_id in call_ids.iter() {
            if let Some(items) = self.server.drain_sip_flow(call_id) {
                if !items.is_empty() {
                    sip_flows_map.insert(call_id.clone(), items);
                }
            }
        }

        let direction = match self.dialplan.direction {
            crate::call::DialDirection::Inbound => "inbound".to_string(),
            crate::call::DialDirection::Outbound => "outbound".to_string(),
            crate::call::DialDirection::Internal => "internal".to_string(),
        };

        // Helper to resolve call status (copied from proxy_call.rs logic)
        let status = if snapshot.answer_time.is_some() {
            "answered".to_string()
        } else if snapshot.last_error.is_some() {
            "failed".to_string()
        } else {
            "missed".to_string()
        };

        let from_number = extract_sip_username(&caller);
        let to_number = extract_sip_username(&callee);

        let trunk_name = self.cookie.get_source_trunk();
        let (sip_gateway, sip_trunk_id) = if let Some(ref name) = trunk_name {
            let trunks = self.server.data_context.trunks_snapshot();
            let trunk_id = trunks.get(name).and_then(|config| config.id);
            (Some(name.clone()), trunk_id)
        } else {
            (None, None)
        };

        let (department_id, extension_id) = if let Some(user) = self.cookie.get_user() {
            let dept_id = user.departments.as_ref().and_then(|deps| {
                deps.iter().find_map(|d| {
                    if d.starts_with("tenant:") {
                        d.trim_start_matches("tenant:").parse::<i64>().ok()
                    } else {
                        d.parse::<i64>().ok()
                    }
                })
            });
            let ext_id = if user.id > 0 {
                Some(user.id as i64)
            } else {
                None
            };
            (dept_id, ext_id)
        } else {
            (None, None)
        };

        let mut recorder = Vec::new();
        if self.dialplan.recording.enabled {
            if let Some(recorder_config) = self.dialplan.recording.option.as_ref() {
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

        let metadata_value = extras_map_to_metadata(&extras_map);
        let recording_path_for_db = recorder.first().map(|media| media.path.clone());

        let mut persist_args = CallRecordPersistArgs::default();
        persist_args.direction = direction;
        persist_args.status = status;
        persist_args.from_number = from_number;
        persist_args.to_number = to_number;
        persist_args.queue = snapshot.last_queue_name.clone();
        persist_args.department_id = department_id;
        persist_args.extension_id = extension_id;
        persist_args.sip_trunk_id = sip_trunk_id;
        persist_args.sip_gateway = sip_gateway;
        persist_args.metadata = metadata_value;
        persist_args.recording_url = recording_path_for_db;

        let mut record = CallRecord {
            call_type: crate::call::ActiveCallType::Sip,
            option: None,
            call_id: self.session_id.clone(),
            start_time,
            ring_time,
            answer_time,
            end_time: now,
            caller: caller.clone(),
            callee: callee.clone(),
            status_code,
            offer: snapshot.caller_offer.clone(),
            answer: snapshot.answer.clone(),
            hangup_reason: hangup_reason.clone(),
            hangup_messages: hangup_messages.clone(),
            recorder,
            extras: None,
            dump_event_file: None,
            refer_callrecord: None,
            sip_flows: sip_flows_map,
            sip_leg_roles,
            persist_args: Some(persist_args),
        };

        apply_record_file_extras(&record, &mut extras_map);
        record.extras = extras_map_to_option(&extras_map);

        if let Some(ref sender) = self.call_record_sender {
            let _ = sender.send(record);
        }
    }
}
