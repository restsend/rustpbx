use crate::models::sip_trunk::{SipTransport, SipTrunkDirection, SipTrunkStatus};
use crate::proxy::routing::RouteQueueConfig;
use sea_orm::{DatabaseConnection, DbErr, Paginator, SelectorTrait};
use serde::{Deserialize, Serialize};
use std::cmp;

#[derive(Deserialize, Default, Clone)]
pub struct LoginQuery {
    pub next: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
pub struct LoginForm {
    pub identifier: String,
    pub password: String,
    pub next: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
pub struct RegisterForm {
    pub email: String,
    pub username: String,
    pub password: String,
    pub confirm_password: String,
}

#[derive(Deserialize, Default, Clone)]
pub struct ForgotForm {
    pub email: String,
}

#[derive(Deserialize, Default, Clone)]
pub struct ResetForm {
    pub password: String,
    pub confirm_password: String,
}

#[derive(Deserialize, Default, Clone)]
pub struct ExtensionPayload {
    pub extension: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub sip_password: Option<String>,
    pub call_forwarding_mode: Option<String>,
    pub call_forwarding_destination: Option<String>,
    pub call_forwarding_timeout: Option<i32>,
    pub notes: Option<String>,
    pub department_ids: Option<Vec<i64>>,
    pub login_disabled: Option<bool>,
    pub voicemail_disabled: Option<bool>,
    pub allow_guest_calls: Option<bool>,
}

#[derive(Deserialize, Default, Clone)]
pub struct SipTrunkForm {
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub carrier: Option<String>,
    pub description: Option<String>,
    pub status: Option<SipTrunkStatus>,
    pub direction: Option<SipTrunkDirection>,
    pub sip_server: Option<String>,
    pub sip_transport: Option<SipTransport>,
    pub outbound_proxy: Option<String>,
    pub auth_username: Option<String>,
    pub auth_password: Option<String>,
    pub default_route_label: Option<String>,
    pub tenant_id: Option<i64>,
    pub clear_tenant: Option<bool>,
    pub max_cps: Option<i32>,
    pub max_concurrent: Option<i32>,
    pub max_call_duration: Option<i32>,
    pub utilisation_percent: Option<f64>,
    pub warning_threshold_percent: Option<f64>,
    pub allowed_ips: Option<String>,
    pub did_numbers: Option<String>,
    pub billing_snapshot: Option<String>,
    pub analytics: Option<String>,
    pub tags: Option<String>,
    pub incoming_from_user_prefix: Option<String>,
    pub incoming_to_user_prefix: Option<String>,
    pub metadata: Option<String>,
    pub is_active: Option<bool>,
}

#[derive(Deserialize, Default, Clone)]
pub struct QueuePayload {
    pub name: Option<String>,
    pub description: Option<String>,
    pub is_active: Option<bool>,
    pub metadata: Option<String>,
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub spec: Option<RouteQueueConfig>,
}

fn default_per_page_min() -> u32 {
    5
}
fn default_per_page_max() -> u32 {
    100
}

#[derive(Deserialize)]
pub struct ListQuery<T> {
    pub page: u32,
    pub per_page: u32,
    #[serde(default = "default_per_page_min")]
    pub per_page_min: u32,
    #[serde(default = "default_per_page_max")]
    pub per_page_max: u32,
    pub filters: Option<T>,
    #[serde(default)]
    pub sort: Option<String>,
}

impl<T: Default> Default for ListQuery<T> {
    fn default() -> Self {
        Self {
            page: 1,
            per_page: 20,
            per_page_min: default_per_page_min(),
            per_page_max: default_per_page_max(),
            filters: None,
            sort: None,
        }
    }
}

impl<T> ListQuery<T> {
    pub fn normalize(&self) -> (u64, u64) {
        let normalized_min = cmp::max(self.per_page_min, 1);
        let normalized_max = cmp::max(self.per_page_max, normalized_min);
        let clamped_per_page = cmp::min(cmp::max(self.per_page, normalized_min), normalized_max);
        let clamped_page = cmp::max(self.page, 1);
        (clamped_page as u64, clamped_per_page as u64)
    }
}

#[derive(Debug, Serialize)]
pub struct Pagination<T> {
    pub items: Vec<T>,
    pub current_page: u32,
    pub per_page: u32,
    pub total_items: u64,
    pub total_pages: u32,
    pub has_prev: bool,
    pub has_next: bool,
}

pub async fn paginate<S, F>(
    paginator: Paginator<'_, DatabaseConnection, S>,
    query: &ListQuery<F>,
) -> Result<Pagination<S::Item>, DbErr>
where
    S: SelectorTrait + Send,
    S::Item: Send,
    F: Default,
{
    let (page, per_page) = query.normalize();
    let total_items = paginator.num_items().await?;
    let total_pages_u64 = if total_items == 0 {
        1
    } else {
        cmp::max((total_items + per_page - 1) / per_page, 1)
    };

    let total_pages = cmp::max(u32::try_from(total_pages_u64).unwrap_or(u32::MAX), 1);
    let requested_page = cmp::max(page, 1);
    let max_page_index = total_pages.saturating_sub(1);
    let page_index_u32 = cmp::min(requested_page.saturating_sub(1), max_page_index as u64);
    let page_index = page_index_u32 as u64;

    let items = paginator.fetch_page(page_index).await?;

    let current_page = (page_index as u32) + 1;
    let has_prev = current_page > 1;
    let has_next = current_page < total_pages;

    Ok(Pagination {
        items,
        current_page,
        per_page: per_page as u32,
        total_items,
        total_pages,
        has_prev,
        has_next,
    })
}
