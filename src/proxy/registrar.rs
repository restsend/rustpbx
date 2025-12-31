use super::{ProxyAction, ProxyModule, server::SipServerRef};
use crate::call::user::SipUser;
use crate::call::{Location, TransactionCookie};
use crate::config::ProxyConfig;
use crate::proxy::locator::LocatorEvent;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use rsip::headers::UntypedHeader;
use rsip::prelude::HeadersExt;
use rsip::{Header, Param, Transport, Uri};
use rsipstack::{transaction::transaction::Transaction, transport::SipAddr};
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

#[derive(Clone)]
pub struct RegistrarModule {
    server: SipServerRef,
    config: Arc<ProxyConfig>,
}

#[derive(Clone, Debug)]
struct ContactParam {
    name: String,
    value: Option<String>,
    quoted: bool,
}

impl ContactParam {
    fn matches(&self, needle: &str) -> bool {
        self.name.eq_ignore_ascii_case(needle)
    }

    fn rendered(&self) -> String {
        match &self.value {
            Some(value) => {
                if self.quoted || value.contains([' ', ';', ',']) {
                    format!("{}=\"{}\"", self.name, value)
                } else {
                    format!("{}={}", self.name, value)
                }
            }
            None => self.name.clone(),
        }
    }

    fn cloned_value(&self) -> Option<String> {
        self.value.clone()
    }
}

#[derive(Clone, Debug)]
struct ParsedContact {
    display_name: Option<String>,
    uri: Uri,
    uri_string: String,
    params: Vec<ContactParam>,
    wildcard: bool,
    has_angle_brackets: bool,
}

impl ParsedContact {
    fn wildcard() -> Self {
        Self {
            display_name: None,
            uri: Uri::default(),
            uri_string: String::new(),
            params: Vec::new(),
            wildcard: true,
            has_angle_brackets: false,
        }
    }

    fn is_wildcard(&self) -> bool {
        self.wildcard
    }

    fn uri(&self) -> &Uri {
        &self.uri
    }

    fn expires(&self) -> Option<u32> {
        self.params
            .iter()
            .find(|p| p.matches("expires"))
            .and_then(|p| p.value.as_ref())
            .and_then(|v| v.parse::<u32>().ok())
    }

    fn instance_id(&self) -> Option<String> {
        self.params
            .iter()
            .find(|p| p.matches("+sip.instance"))
            .and_then(ContactParam::cloned_value)
    }

    fn gruu(&self) -> Option<String> {
        self.params
            .iter()
            .find(|p| p.matches("pub-gruu"))
            .and_then(ContactParam::cloned_value)
    }

    fn temp_gruu(&self) -> Option<String> {
        self.params
            .iter()
            .find(|p| p.matches("temp-gruu"))
            .and_then(ContactParam::cloned_value)
    }

    fn reg_id(&self) -> Option<String> {
        self.params
            .iter()
            .find(|p| p.matches("reg-id"))
            .and_then(ContactParam::cloned_value)
    }

    fn transport(&self) -> Option<Transport> {
        if let Some(param) = self.params.iter().find(|p| p.matches("transport")) {
            if let Some(value) = &param.value {
                if let Some(t) = parse_transport_token(value) {
                    return Some(t);
                }
            }
        }

        self.uri.params.iter().find_map(|param| match param {
            Param::Transport(t) => Some(*t),
            _ => None,
        })
    }

    fn destination_from_uri(&self) -> Option<SipAddr> {
        Some(SipAddr {
            r#type: self.transport(),
            addr: self.uri.host_with_port.clone(),
        })
    }

    fn contact_value(&self, expires: u32) -> String {
        if self.wildcard {
            return "*".to_string();
        }

        let mut value = String::new();
        if let Some(display) = &self.display_name {
            value.push('"');
            value.push_str(&escape_display_name(display));
            value.push('"');
            value.push(' ');
        }
        if self.has_angle_brackets {
            value.push('<');
            value.push_str(&self.uri_string);
            value.push('>');
        } else {
            value.push_str(&self.uri_string);
        }

        for param in self.params.iter() {
            if param.matches("expires") {
                continue;
            }
            value.push(';');
            value.push_str(&param.rendered());
        }
        value.push_str(&format!(";expires={}", expires));
        value
    }

    fn param_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for param in self.params.iter() {
            if param.matches("expires") {
                continue;
            }
            map.insert(
                param.name.to_ascii_lowercase(),
                param.value.clone().unwrap_or_default(),
            );
        }
        map
    }
}

fn parse_contact_headers(request: &rsip::Request) -> Result<Vec<ParsedContact>> {
    let mut contacts = Vec::new();
    for value in collect_header_values(request, "Contact") {
        for entry in split_contact_entries(&value) {
            if entry.is_empty() {
                continue;
            }
            contacts.push(parse_contact_entry(&entry)?);
        }
    }
    Ok(contacts)
}

fn collect_header_values(request: &rsip::Request, name: &str) -> Vec<String> {
    let header_name = name.to_ascii_lowercase();
    request
        .headers
        .iter()
        .filter_map(|header| {
            let text = header.to_string();
            let mut parts = text.splitn(2, ':');
            let header_key = parts.next()?.trim().to_ascii_lowercase();
            if header_key == header_name {
                let value = parts.next().unwrap_or("").trim().to_string();
                Some(value)
            } else {
                None
            }
        })
        .collect()
}

fn split_contact_entries(value: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut angle_depth = 0;
    for (idx, ch) in value.char_indices() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
            }
            '<' if !in_quotes => {
                angle_depth += 1;
            }
            '>' if !in_quotes && angle_depth > 0 => {
                angle_depth -= 1;
            }
            ',' if !in_quotes && angle_depth == 0 => {
                let segment = value[start..idx].trim();
                if !segment.is_empty() {
                    entries.push(segment.to_string());
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let last = value[start..].trim();
    if !last.is_empty() {
        entries.push(last.to_string());
    }
    entries
}

fn parse_contact_entry(entry: &str) -> Result<ParsedContact> {
    let trimmed = entry.trim();
    if trimmed.eq("*") {
        return Ok(ParsedContact::wildcard());
    }

    let mut remainder = trimmed;
    let mut display_name = None;
    let mut has_angle_brackets = false;

    if remainder.starts_with('"') {
        if let Some(relative_end) = remainder[1..].find('"') {
            let end_idx = 1 + relative_end;
            display_name = Some(remainder[1..end_idx].to_string());
            remainder = remainder.get(end_idx + 1..).unwrap_or("").trim();
        }
    } else if let Some(angle_pos) = remainder.find('<') {
        let maybe_name = remainder[..angle_pos].trim();
        if !maybe_name.is_empty() {
            display_name = Some(maybe_name.trim_matches('"').to_string());
        }
        remainder = remainder[angle_pos..].trim();
    }

    let (uri_part, param_part) = if remainder.starts_with('<') {
        let end_idx = remainder
            .find('>')
            .ok_or_else(|| anyhow!("contact header missing '>'"))?;
        let uri = remainder[1..end_idx].trim();
        let params = remainder[end_idx + 1..].trim();
        has_angle_brackets = true;
        (uri.to_string(), params)
    } else {
        let mut segments = remainder.splitn(2, ';');
        let uri = segments
            .next()
            .ok_or_else(|| anyhow!("malformed contact entry"))?
            .trim()
            .to_string();
        let params = segments.next().unwrap_or("");
        (uri, params)
    };

    if uri_part == "*" {
        return Ok(ParsedContact::wildcard());
    }

    let uri = Uri::try_from(uri_part.as_str())
        .map_err(|e| anyhow!("failed to parse contact uri '{}': {}", uri_part, e))?;
    let params = parse_contact_params(param_part);

    Ok(ParsedContact {
        display_name,
        uri,
        uri_string: uri_part,
        params,
        wildcard: false,
        has_angle_brackets,
    })
}

fn parse_contact_params(segment: &str) -> Vec<ContactParam> {
    let mut params = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in segment.trim().chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ';' if !in_quotes => {
                if !current.trim().is_empty() {
                    params.push(build_contact_param(current.trim()));
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        params.push(build_contact_param(current.trim()));
    }

    params.into_iter().flatten().collect()
}

fn build_contact_param(value: &str) -> Option<ContactParam> {
    let cleaned = value.trim().trim_start_matches(';').trim();
    if cleaned.is_empty() {
        return None;
    }
    if let Some(eq_idx) = cleaned.find('=') {
        let name = cleaned[..eq_idx].trim().to_string();
        let raw_value = cleaned[eq_idx + 1..].trim();
        let (val, quoted) =
            if raw_value.starts_with('"') && raw_value.ends_with('"') && raw_value.len() >= 2 {
                (Some(raw_value[1..raw_value.len() - 1].to_string()), true)
            } else {
                (Some(raw_value.to_string()), false)
            };
        Some(ContactParam {
            name,
            value: val,
            quoted,
        })
    } else {
        Some(ContactParam {
            name: cleaned.to_string(),
            value: None,
            quoted: false,
        })
    }
}

fn parse_route_header(request: &rsip::Request, header: &str) -> Vec<Uri> {
    collect_header_values(request, header)
        .into_iter()
        .flat_map(|value| {
            split_contact_entries(&value)
                .into_iter()
                .filter_map(|entry| {
                    let trimmed = entry.trim();
                    if trimmed.is_empty() {
                        return None;
                    }
                    let stripped = trimmed.trim_start_matches('<').trim_end_matches('>').trim();
                    Uri::try_from(stripped).ok()
                })
        })
        .collect()
}

fn parse_transport_token(value: &str) -> Option<Transport> {
    match value.to_ascii_lowercase().as_str() {
        "udp" => Some(Transport::Udp),
        "tcp" => Some(Transport::Tcp),
        "tls" => Some(Transport::Tls),
        "ws" => Some(Transport::Ws),
        "wss" => Some(Transport::Wss),
        _ => None,
    }
}

fn escape_display_name(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_contact_with_gruu_and_instance() {
        let value = "\"Alice\" <sip:alice@rustpbx.com;transport=ws>;expires=600;+sip.instance=\"urn:uuid:1234\";pub-gruu=\"sip:alice-gruu@rustpbx.com\"";
        let parsed = parse_contact_entry(value).expect("parse contact");
        assert!(!parsed.is_wildcard());
        assert_eq!(parsed.expires(), Some(600));
        assert_eq!(parsed.instance_id().as_deref(), Some("urn:uuid:1234"));
        assert_eq!(parsed.gruu().as_deref(), Some("sip:alice-gruu@rustpbx.com"));
        assert!(matches!(parsed.transport(), Some(Transport::Ws)));
        let rendered = parsed.contact_value(300);
        assert!(rendered.contains("+sip.instance=\"urn:uuid:1234\""));
        assert!(rendered.contains("expires=300"));
    }

    #[test]
    fn parse_contact_wildcard() {
        let parsed = parse_contact_entry("*").expect("parse wildcard");
        assert!(parsed.is_wildcard());
    }

    #[test]
    fn parse_contact_without_angle_brackets() {
        let value = "sip:alice@rustpbx.com;transport=ws;expires=120";
        let parsed = parse_contact_entry(value).expect("parse contact");
        assert!(!parsed.is_wildcard());
        let rendered = parsed.contact_value(60);
        assert!(rendered.starts_with("sip:alice@rustpbx.com"));
        assert!(!rendered.contains('<'));
        assert!(!rendered.contains('>'));
        assert!(rendered.contains("transport=ws"));
        assert!(rendered.contains("expires=60"));
    }
}

impl RegistrarModule {
    pub fn create(server: SipServerRef, config: Arc<ProxyConfig>) -> Result<Box<dyn ProxyModule>> {
        let module = RegistrarModule::new(server, config);
        Ok(Box::new(module))
    }
    pub fn new(server: SipServerRef, config: Arc<ProxyConfig>) -> Self {
        Self { server, config }
    }
}

#[async_trait]
impl ProxyModule for RegistrarModule {
    fn name(&self) -> &str {
        "registrar"
    }
    fn allow_methods(&self) -> Vec<rsip::Method> {
        vec![rsip::Method::Register]
    }
    async fn on_start(&mut self) -> Result<()> {
        debug!("Registrar module started");
        Ok(())
    }
    async fn on_stop(&self) -> Result<()> {
        debug!("Registrar module stopped");
        Ok(())
    }

    async fn on_transaction_begin(
        &self,
        _token: CancellationToken,
        tx: &mut Transaction,
        _cookie: TransactionCookie,
    ) -> Result<ProxyAction> {
        let method = tx.original.method();
        if !matches!(method, rsip::Method::Register) {
            return Ok(ProxyAction::Continue);
        }

        let user = match SipUser::try_from(&*tx) {
            Ok(u) => u,
            Err(e) => {
                info!("failed to parse user: {:?}", e);
                tx.reply(rsip::StatusCode::BadRequest).await.ok();
                return Ok(ProxyAction::Abort);
            }
        };

        let registered_aor = match tx.original.to_header() {
            Ok(to) => match to.uri() {
                Ok(uri) => uri,
                Err(e) => {
                    info!("invalid To header uri: {:?}", e);
                    tx.reply(rsip::StatusCode::BadRequest).await.ok();
                    return Ok(ProxyAction::Abort);
                }
            },
            Err(e) => {
                info!("missing To header: {:?}", e);
                tx.reply(rsip::StatusCode::BadRequest).await.ok();
                return Ok(ProxyAction::Abort);
            }
        };

        let default_expires = self.config.registrar_expires.unwrap_or(60);
        let global_expires = tx
            .original
            .expires_header()
            .and_then(|header| header.value().parse::<u32>().ok());

        let contact_entries = match parse_contact_headers(&tx.original) {
            Ok(entries) => entries,
            Err(e) => {
                info!("failed to parse contact headers: {:?}", e);
                tx.reply(rsip::StatusCode::BadRequest).await.ok();
                return Ok(ProxyAction::Abort);
            }
        };

        if contact_entries.is_empty() {
            info!("REGISTER missing Contact header");
            tx.reply(rsip::StatusCode::BadRequest).await.ok();
            return Ok(ProxyAction::Abort);
        }

        let wildcard_count = contact_entries.iter().filter(|c| c.is_wildcard()).count();
        if wildcard_count > 0 {
            if wildcard_count > 1 || contact_entries.len() > 1 {
                info!("invalid REGISTER with multiple wildcard contacts");
                tx.reply(rsip::StatusCode::BadRequest).await.ok();
                return Ok(ProxyAction::Abort);
            }
            let expires = contact_entries[0].expires().or(global_expires).unwrap_or(0);
            if expires != 0 {
                info!("wildcard contact must have expires=0");
                tx.reply(rsip::StatusCode::BadRequest).await.ok();
                return Ok(ProxyAction::Abort);
            }

            self.server
                .locator
                .unregister(user.username.as_str(), user.realm.as_deref())
                .await
                .ok();

            if let Some(locator_events) = &self.server.locator_events {
                locator_events
                    .send(LocatorEvent::Unregistered(Location {
                        aor: registered_aor.clone(),
                        registered_aor: Some(registered_aor),
                        ..Default::default()
                    }))
                    .ok();
            }

            let mut headers = Vec::new();
            if let Some(allows) = tx.endpoint_inner.allows.lock().unwrap().as_ref() {
                if !allows.is_empty() {
                    headers.push(rsip::Header::Allow(
                        allows
                            .iter()
                            .map(|m| m.to_string())
                            .collect::<Vec<String>>()
                            .join(",")
                            .into(),
                    ));
                }
            }
            tx.reply_with(rsip::StatusCode::OK, headers, None)
                .await
                .ok();
            return Ok(ProxyAction::Abort);
        }

        let path_uris = parse_route_header(&tx.original, "Path");
        let service_routes = parse_route_header(&tx.original, "Service-Route");

        let mut response_headers = Vec::new();
        for entry in contact_entries {
            let expires = entry
                .expires()
                .or(global_expires)
                .unwrap_or(default_expires);

            let destination = match user
                .destination
                .clone()
                .or_else(|| entry.destination_from_uri())
            {
                Some(dest) => dest,
                None => {
                    info!("unable to determine network destination for REGISTER");
                    tx.reply(rsip::StatusCode::ServiceUnavailable).await.ok();
                    return Ok(ProxyAction::Abort);
                }
            };

            let rendered_contact = entry.contact_value(expires);
            let mut location = Location {
                aor: entry.uri().clone(),
                expires,
                destination: Some(destination.clone()),
                last_modified: Some(Instant::now()),
                supports_webrtc: {
                    let is_ws = entry
                        .transport()
                        .map(|t| matches!(t, Transport::Ws | Transport::Wss))
                        .unwrap_or(false);
                    let is_invalid = entry.uri().host().to_string().contains(".invalid");
                    (is_ws && is_invalid) || user.is_support_webrtc
                },
                credential: None,
                headers: None,
                registered_aor: Some(registered_aor.clone()),
                contact_raw: Some(rendered_contact.clone()),
                contact_params: Some(entry.param_map()),
                path: (!path_uris.is_empty()).then_some(path_uris.clone()),
                service_route: (!service_routes.is_empty()).then_some(service_routes.clone()),
                instance_id: entry.instance_id(),
                gruu: entry.gruu(),
                temp_gruu: entry.temp_gruu(),
                reg_id: entry.reg_id(),
                transport: entry.transport(),
                user_agent: tx
                    .original
                    .user_agent_header()
                    .map(|header| header.value().to_string()),
            };

            if location.transport.is_none() {
                location.transport = destination.r#type;
            }

            match self
                .server
                .locator
                .register(
                    user.username.as_str(),
                    user.realm.as_deref(),
                    location.clone(),
                )
                .await
            {
                Ok(_) => {
                    if let Some(locator_events) = &self.server.locator_events {
                        locator_events.send(LocatorEvent::Registered(location)).ok();
                    }
                }
                Err(e) => {
                    info!("failed to register user: {:?}", e);
                    tx.reply(rsip::StatusCode::ServiceUnavailable).await.ok();
                    return Ok(ProxyAction::Abort);
                }
            }

            response_headers.push(Header::Other("Contact".into(), rendered_contact.into()));
        }

        if let Some(allows) = tx.endpoint_inner.allows.lock().unwrap().as_ref() {
            if !allows.is_empty() {
                response_headers.push(Header::Allow(
                    allows
                        .iter()
                        .map(|m| m.to_string())
                        .collect::<Vec<String>>()
                        .join(",")
                        .into(),
                ));
            }
        }
        response_headers.push(Header::Expires(
            global_expires.unwrap_or(default_expires).into(),
        ));

        tx.reply_with(rsip::StatusCode::OK, response_headers, None)
            .await
            .ok();
        Ok(ProxyAction::Abort)
    }
}
