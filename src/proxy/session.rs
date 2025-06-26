use std::time::Instant;

use rsipstack::dialog::DialogId;

#[derive(Clone, Debug)]
pub(crate) struct SessionParty {
    pub aor: rsip::Uri, // Address of Record
}

impl SessionParty {
    pub fn new(aor: rsip::Uri) -> Self {
        Self { aor }
    }

    pub fn get_user(&self) -> String {
        self.aor.user().unwrap_or_default().to_string()
    }

    pub fn get_realm(&self) -> String {
        self.aor.host().to_string()
    }
}

#[derive(Clone)]
pub(crate) struct Session {
    pub dialog_id: DialogId,
    pub last_activity: Instant,
    pub caller: SessionParty,
    pub callees: Vec<SessionParty>,
}
