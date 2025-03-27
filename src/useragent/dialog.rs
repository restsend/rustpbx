use anyhow::{anyhow, Result};
use rsip::{
    self,
    common::uri::param::Param,
    headers::{self, Header, Headers, UntypedHeader},
    message::{Request, SipMessage},
    Method, StatusCode, Uri,
};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::Mutex;
use tracing::debug;
use uuid::Uuid;

/// SIP Dialog State
#[derive(Debug, Clone, PartialEq)]
pub enum DialogState {
    /// Initial state
    Initial,
    /// Early dialog state (received 1xx response)
    Early,
    /// Confirmed dialog state (received 2xx response)
    Confirmed,
    /// Terminated dialog state
    Terminated,
}

/// SIP Dialog
#[derive(Debug)]
pub struct SipDialog {
    /// Dialog ID
    pub id: String,
    /// Local URI
    pub local_uri: Uri,
    /// Remote URI
    pub remote_uri: Uri,
    /// Call ID
    pub call_id: String,
    /// Local tag
    pub local_tag: String,
    /// Remote tag
    pub remote_tag: Option<String>,
    /// CSeq number
    pub cseq: u32,
    /// Dialog state
    pub state: DialogState,
    /// Creation time
    pub created_at: SystemTime,
}

impl SipDialog {
    /// Create a new SIP dialog
    pub fn new(local_uri_str: &str, remote_uri_str: &str) -> Result<Self> {
        // Parse URIs
        let local_uri = Uri::try_from(local_uri_str)?;
        let remote_uri = Uri::try_from(remote_uri_str)?;

        // Generate unique dialog ID
        let id = format!("dialog-{}", Uuid::new_v4());

        // Generate call ID
        let call_id = format!("call-{}", Uuid::new_v4());

        // Generate local tag
        let local_tag = format!("tag-{}", Uuid::new_v4());

        Ok(Self {
            id,
            local_uri,
            remote_uri,
            call_id,
            local_tag,
            remote_tag: None,
            cseq: 1,
            state: DialogState::Initial,
            created_at: SystemTime::now(),
        })
    }

    /// Create INVITE request
    pub fn create_invite(&mut self, sdp: Option<&str>) -> Result<SipMessage> {
        // Create basic headers
        let mut headers = Headers::default();

        // Add Call-ID header
        headers.push(Header::CallId(self.call_id.clone().into()));

        // Add CSeq header
        headers.push(Header::CSeq(
            format!("{} {}", self.cseq, Method::Invite).into(),
        ));

        // Add From header with local tag
        let local_uri_str = self.local_uri.to_string();
        let mut from = Header::From(local_uri_str.into());

        // Create a request
        let mut request = Request {
            method: Method::Invite,
            uri: self.remote_uri.clone(),
            headers,
            version: rsip::Version::V2,
            body: Vec::new(),
        };

        // Add From with tag parameter
        request.headers.push(Header::From(
            format!("<{}>;tag={}", self.local_uri, self.local_tag).into(),
        ));

        // Add To header
        request
            .headers
            .push(Header::To(format!("<{}>", self.remote_uri).into()));

        // If SDP is provided, add content
        if let Some(sdp_content) = sdp {
            request
                .headers
                .push(Header::ContentType("application/sdp".into()));
            request
                .headers
                .push(Header::ContentLength(sdp_content.len().to_string().into()));
            request.body = sdp_content.as_bytes().to_vec();
        }

        // Increment CSeq for next request
        self.cseq += 1;

        // Wrap as SIP message
        Ok(SipMessage::Request(request))
    }

    /// Handle response
    pub fn handle_response(&mut self, response: &SipMessage) -> Result<()> {
        if let SipMessage::Response(resp) = response {
            let status_code_val = u16::from(resp.status_code.clone());

            // Handle 2xx responses (success)
            if status_code_val == 200 {
                // Extract tag from To header
                if let Some(to_header) = resp.headers.iter().find(|h| matches!(h, Header::To(_))) {
                    if let Header::To(to) = to_header {
                        // Extract tag from To header string
                        let to_str = to.to_string();
                        if let Some(tag_idx) = to_str.find("tag=") {
                            let tag_start = tag_idx + 4; // Skip "tag="
                            let tag_end = to_str[tag_start..]
                                .find(&[';', '>', ' '][..])
                                .map(|idx| tag_start + idx)
                                .unwrap_or_else(|| to_str.len());

                            self.remote_tag = Some(to_str[tag_start..tag_end].to_string());
                        }
                    }
                }

                // Update dialog state to confirmed
                self.state = DialogState::Confirmed;
                debug!("Dialog confirmed: {}", self.id);
            // Handle 1xx responses (provisional)
            } else if status_code_val >= 180 && status_code_val < 200 {
                // Update dialog state to early
                self.state = DialogState::Early;
                debug!("Dialog in early state: {}", self.id);
            } else if status_code_val >= 300 {
                // Update dialog state to terminated
                self.state = DialogState::Terminated;
                debug!("Dialog terminated: {}", self.id);
            }
        }

        Ok(())
    }

    /// Create BYE request
    pub fn create_bye(&mut self) -> Result<SipMessage> {
        // Create request
        let mut request = Request {
            method: Method::Bye,
            uri: self.remote_uri.clone(),
            headers: Headers::default(),
            version: rsip::Version::V2,
            body: Vec::new(),
        };

        // Add Call-ID header
        request
            .headers
            .push(Header::CallId(self.call_id.clone().into()));

        // Add CSeq header
        request.headers.push(Header::CSeq(
            format!("{} {}", self.cseq, Method::Bye).into(),
        ));

        // Add From header with tag
        request.headers.push(Header::From(
            format!("<{}>;tag={}", self.local_uri, self.local_tag).into(),
        ));

        // Add To header with remote tag if available
        let to_header = if let Some(tag) = &self.remote_tag {
            format!("<{}>;tag={}", self.remote_uri, tag)
        } else {
            format!("<{}>", self.remote_uri)
        };
        request.headers.push(Header::To(to_header.into()));

        // Update dialog state
        self.state = DialogState::Terminated;

        // Increment CSeq for next request
        self.cseq += 1;

        // Wrap as SIP message
        Ok(SipMessage::Request(request))
    }

    /// Create ACK request
    pub fn create_ack(&mut self) -> Result<SipMessage> {
        // Create request
        let mut request = Request {
            method: Method::Ack,
            uri: self.remote_uri.clone(),
            headers: Headers::default(),
            version: rsip::Version::V2,
            body: Vec::new(),
        };

        // Add Call-ID header
        request
            .headers
            .push(Header::CallId(self.call_id.clone().into()));

        // Add CSeq header for ACK (using same CSeq as INVITE)
        request.headers.push(Header::CSeq(
            format!("{} {}", self.cseq - 1, Method::Ack).into(),
        ));

        // Add From header with tag
        request.headers.push(Header::From(
            format!("<{}>;tag={}", self.local_uri, self.local_tag).into(),
        ));

        // Add To header with remote tag if available
        let to_header = if let Some(tag) = &self.remote_tag {
            format!("<{}>;tag={}", self.remote_uri, tag)
        } else {
            format!("<{}>", self.remote_uri)
        };
        request.headers.push(Header::To(to_header.into()));

        // Wrap as SIP message
        Ok(SipMessage::Request(request))
    }
}

/// Dialog Manager
#[derive(Debug, Default)]
pub struct DialogManager {
    /// Dialog map indexed by dialog ID
    dialogs_by_id: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<SipDialog>>>>>,
    /// Dialog map indexed by Call-ID
    dialogs_by_call_id: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<SipDialog>>>>>,
}

impl DialogManager {
    /// Create a new dialog manager
    pub fn new() -> Self {
        Self {
            dialogs_by_id: Arc::new(Mutex::new(std::collections::HashMap::new())),
            dialogs_by_call_id: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Create a new dialog
    pub async fn create_dialog(
        &self,
        local_uri: &str,
        remote_uri: &str,
    ) -> Result<Arc<Mutex<SipDialog>>> {
        let dialog = SipDialog::new(local_uri, remote_uri)?;
        let id = dialog.id.clone();
        let call_id = dialog.call_id.clone();

        let dialog = Arc::new(Mutex::new(dialog));

        // Save dialog references
        self.dialogs_by_id.lock().await.insert(id, dialog.clone());
        self.dialogs_by_call_id
            .lock()
            .await
            .insert(call_id, dialog.clone());

        Ok(dialog)
    }

    /// Find dialog by ID
    pub async fn find_by_id(&self, id: &str) -> Option<Arc<Mutex<SipDialog>>> {
        self.dialogs_by_id.lock().await.get(id).cloned()
    }

    /// Find dialog by Call-ID
    pub async fn find_by_call_id(&self, call_id: &str) -> Option<Arc<Mutex<SipDialog>>> {
        self.dialogs_by_call_id.lock().await.get(call_id).cloned()
    }

    /// Close dialog
    pub async fn close_dialog(&self, id: &str) -> Result<()> {
        let call_id = if let Some(dialog) = self.dialogs_by_id.lock().await.get(id) {
            let d = dialog.lock().await;
            Some(d.call_id.clone())
        } else {
            None
        };

        if let Some(call_id) = call_id {
            self.dialogs_by_id.lock().await.remove(id);
            self.dialogs_by_call_id.lock().await.remove(&call_id);
        }

        Ok(())
    }
}
