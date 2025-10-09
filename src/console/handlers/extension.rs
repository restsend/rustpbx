use crate::console::{ConsoleState, middleware::AuthRequired};
use axum::{
    extract::{Path as AxumPath, State},
    response::Response,
};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone, Serialize)]
struct ExtensionSummary {
    id: &'static str,
    extension: &'static str,
    display_name: &'static str,
    department: &'static str,
    status: &'static str,
    login_allowed: bool,
    created_at: &'static str,
    registered_at: Option<&'static str>,
    registrar: Option<&'static str>,
}

#[derive(Clone, Serialize)]
struct ExtensionDetailMock {
    id: &'static str,
    extension: &'static str,
    display_name: &'static str,
    department: &'static str,
    email: Option<&'static str>,
    login_allowed: bool,
    sip_password: &'static str,
    voicemail_enabled: bool,
    caller_id_name: &'static str,
    caller_id_number: &'static str,
    outbound_caller_id: Option<&'static str>,
    emergency_caller_id: Option<&'static str>,
    pin: Option<&'static str>,
    notes: Option<&'static str>,
    registered_at: Option<&'static str>,
    created_at: &'static str,
}

fn sample_extension_summaries() -> Vec<ExtensionSummary> {
    vec![
        ExtensionSummary {
            id: "1001",
            extension: "1001",
            display_name: "Alice Johnson",
            department: "Support",
            status: "Online",
            login_allowed: true,
            created_at: "2025-01-12",
            registered_at: Some("2025-03-01T11:45:00Z"),
            registrar: Some("10.10.10.21"),
        },
        ExtensionSummary {
            id: "1002",
            extension: "1002",
            display_name: "Brian Clark",
            department: "Sales",
            status: "Offline",
            login_allowed: false,
            created_at: "2024-11-05",
            registered_at: None,
            registrar: None,
        },
        ExtensionSummary {
            id: "1003",
            extension: "1003",
            display_name: "Carla Gomez",
            department: "R&D",
            status: "Online",
            login_allowed: true,
            created_at: "2025-02-18",
            registered_at: Some("2025-03-04T08:12:00Z"),
            registrar: Some("10.10.12.5"),
        },
        ExtensionSummary {
            id: "1004",
            extension: "1004",
            display_name: "David Wu",
            department: "Finance",
            status: "Idle",
            login_allowed: true,
            created_at: "2024-09-01",
            registered_at: Some("2025-02-28T16:05:00Z"),
            registrar: Some("192.168.2.45"),
        },
        ExtensionSummary {
            id: "1005",
            extension: "1005",
            display_name: "Emilia Rossi",
            department: "Support",
            status: "Ringing",
            login_allowed: true,
            created_at: "2024-12-20",
            registered_at: Some("2025-03-03T09:30:00Z"),
            registrar: Some("100.64.10.2"),
        },
        ExtensionSummary {
            id: "1006",
            extension: "1006",
            display_name: "Fletcher Grant",
            department: "Operations",
            status: "Offline",
            login_allowed: true,
            created_at: "2024-10-14",
            registered_at: None,
            registrar: None,
        },
    ]
}

fn sample_extension_detail(id: &str) -> Option<ExtensionDetailMock> {
    match id {
        "1001" => Some(ExtensionDetailMock {
            id: "1001",
            extension: "1001",
            display_name: "Alice Johnson",
            department: "Support",
            email: Some("alice.johnson@example.com"),
            login_allowed: true,
            sip_password: "••••••••",
            voicemail_enabled: true,
            caller_id_name: "Alice Johnson",
            caller_id_number: "+1 555 010 1001",
            outbound_caller_id: Some("+1 555 555 0101"),
            emergency_caller_id: Some("+1 555 911 0101"),
            pin: Some("7481"),
            notes: Some("Primary support agent for tier 2 escalations."),
            registered_at: Some("2025-03-01T11:45:00Z"),
            created_at: "2025-01-12",
        }),
        "1002" => Some(ExtensionDetailMock {
            id: "1002",
            extension: "1002",
            display_name: "Brian Clark",
            department: "Sales",
            email: Some("brian.clark@example.com"),
            login_allowed: false,
            sip_password: "••••••••",
            voicemail_enabled: false,
            caller_id_name: "Brian Clark",
            caller_id_number: "+1 555 010 1002",
            outbound_caller_id: Some("+1 555 555 0102"),
            emergency_caller_id: None,
            pin: None,
            notes: Some("Disabled login pending compliance training."),
            registered_at: None,
            created_at: "2024-11-05",
        }),
        "1003" => Some(ExtensionDetailMock {
            id: "1003",
            extension: "1003",
            display_name: "Carla Gomez",
            department: "R&D",
            email: Some("carla.gomez@example.com"),
            login_allowed: true,
            sip_password: "••••••••",
            voicemail_enabled: true,
            caller_id_name: "Carla Gomez",
            caller_id_number: "+34 91 555 1003",
            outbound_caller_id: None,
            emergency_caller_id: None,
            pin: Some("3920"),
            notes: Some("Leads prototype demo calls."),
            registered_at: Some("2025-03-04T08:12:00Z"),
            created_at: "2025-02-18",
        }),
        _ => None,
    }
}

fn blank_extension_detail(id: Option<&str>) -> serde_json::Value {
    let extension_number = id.unwrap_or("");
    json!({
        "id": id,
        "extension": extension_number,
        "display_name": "",
        "department": "",
        "email": "",
        "login_allowed": true,
        "sip_password": "",
        "voicemail_enabled": false,
        "caller_id_name": "",
        "caller_id_number": "",
        "outbound_caller_id": "",
        "emergency_caller_id": "",
        "pin": "",
        "notes": "",
        "registered_at": null,
        "created_at": "",
    })
}

pub async fn page_extensions(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let list = sample_extension_summaries();
    let extensions: Vec<_> = list
        .iter()
        .map(|item| {
            let detail_url = state.url_for(&format!("/extensions/{}", item.id));
            let edit_url = detail_url.clone();
            let toggle_url = state.url_for(&format!("/extensions/{}/toggle", item.id));
            let delete_url = state.url_for(&format!("/extensions/{}/delete", item.id));
            json!({
                "id": item.id,
                "extension": item.extension,
                "display_name": item.display_name,
                "department": item.department,
                "status": item.status,
                "login_allowed": item.login_allowed,
                "created_at": item.created_at,
                "registered_at": item.registered_at,
                "registrar": item.registrar,
                "detail_url": detail_url,
                "edit_url": edit_url,
                "toggle_url": toggle_url,
                "delete_url": delete_url,
            })
        })
        .collect();

    let per_page = 10u32;
    let current_page = 1u32;
    let total_records = 42u32;
    let total_pages = (total_records + per_page - 1) / per_page;
    let showing_from = (current_page - 1) * per_page + 1;
    let showing_to = showing_from + extensions.len() as u32 - 1;

    let filters_context = json!({
        "available": [
            {
                "id": "department",
                "label": "Department",
                "type": "select",
                "options": ["Support", "Sales", "R&D", "Finance", "Operations"],
            },
            {
                "id": "created_at",
                "label": "Created time",
                "type": "daterange",
            },
            {
                "id": "login_allowed",
                "label": "Login allowed",
                "type": "select",
                "options": ["Allowed", "Blocked"],
            },
            {
                "id": "registered_at",
                "label": "Registration time",
                "type": "daterange",
            },
        ],
        "active": [
            {
                "id": "department",
                "label": "Department",
                "type": "select",
                "value": ["Support"],
            },
            {
                "id": "login_allowed",
                "label": "Login allowed",
                "type": "select",
                "value": ["Allowed"],
            }
        ],
    });

    state.render(
        "console/extensions.html",
        json!({
            "nav_active": "extensions",
            "extensions": extensions,
            "pagination": {
                "current_page": current_page,
                "per_page": per_page,
                "total_records": total_records,
                "total_pages": total_pages,
                "showing_from": showing_from,
                "showing_to": showing_to,
                "has_prev": current_page > 1,
                "has_next": current_page < total_pages,
                "prev_page": if current_page > 1 { current_page - 1 } else { 1 },
                "next_page": if current_page < total_pages { current_page + 1 } else { total_pages },
            },
            "filters": filters_context,
            "create_url": state.url_for("/extensions/new"),
        }),
    )
}

pub async fn page_extension_create(
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let department_options = vec!["Support", "Sales", "R&D", "Finance", "Operations"];
    let payload = blank_extension_detail(None);

    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "mode": "create",
            "page_title": "Create extension",
            "submit_label": "Create extension",
            "extension": payload,
            "department_options": department_options,
            "form_action": state.url_for("/extensions"),
            "back_url": state.url_for("/extensions"),
            "login_states": [
                {"value": true, "label": "Allowed"},
                {"value": false, "label": "Blocked"},
            ],
        }),
    )
}

pub async fn page_extension_detail(
    AxumPath(id): AxumPath<String>,
    State(state): State<Arc<ConsoleState>>,
    AuthRequired(_): AuthRequired,
) -> Response {
    let department_options = vec!["Support", "Sales", "R&D", "Finance", "Operations"];
    let detail = sample_extension_detail(&id)
        .map(|mock| {
            serde_json::to_value(mock).unwrap_or_else(|_| blank_extension_detail(Some(id.as_str())))
        })
        .unwrap_or_else(|| blank_extension_detail(Some(id.as_str())));

    state.render(
        "console/extension_detail.html",
        json!({
            "nav_active": "extensions",
            "mode": "edit",
            "page_title": format!("Extension {}", detail.get("extension").and_then(|v| v.as_str()).unwrap_or(&id)),
            "submit_label": "Save changes",
            "extension": detail,
            "department_options": department_options,
            "form_action": state.url_for(&format!("/extensions/{}", &id)),
            "back_url": state.url_for("/extensions"),
            "login_states": [
                {"value": true, "label": "Allowed"},
                {"value": false, "label": "Blocked"},
            ],
        }),
    )
}
