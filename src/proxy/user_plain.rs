use super::user::UserBackend;
use crate::call::user::SipUser;
use crate::proxy::auth::AuthError;
use anyhow::Result;
use async_trait::async_trait;
use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::{collections::HashMap, fs::File, io, path::Path};
use tracing::{info, warn};

pub struct PlainTextBackend {
    users: Arc<Mutex<HashMap<String, SipUser>>>,
    path: String,
}

impl PlainTextBackend {
    pub fn new(path: &str) -> Self {
        Self {
            users: Arc::new(Mutex::new(HashMap::new())),
            path: path.to_owned(),
        }
    }

    pub async fn load(&self) -> Result<()> {
        let path = Path::new(&self.path);
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) => {
                warn!(
                    "Files not found, creating empty file: {} {}",
                    e,
                    path.display()
                );
                return Err(e.into());
            }
        };
        let reader = io::BufReader::new(file);
        let mut users = self.users.lock().unwrap();
        users.clear();

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let parts: Vec<&str> = line.splitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }

            let username = parts[0].trim();
            let password = parts[1].trim();
            let user = SipUser {
                id: 0,
                username: username.to_string(),
                password: Some(password.to_string()),
                enabled: true,
                realm: None,
                origin_contact: None,
                contact: None,
                from: None,
                destination: None,
                is_support_webrtc: false,
                call_forwarding_mode: None,
                call_forwarding_destination: None,
                call_forwarding_timeout: None,
                departments: None,
                display_name: None,
                email: None,
                phone: None,
                note: None,
                allow_guest_calls: false,
            };
            users.insert(username.to_string(), user);
        }
        info!("Loaded {} users from {}", users.len(), self.path);
        Ok(())
    }
}

#[async_trait]
impl UserBackend for PlainTextBackend {
    async fn is_same_realm(&self, realm: &str) -> bool {
        return realm.is_empty();
    }
    async fn get_user(
        &self,
        username: &str,
        realm: Option<&str>,
        _request: Option<&rsip::Request>,
    ) -> Result<Option<SipUser>, AuthError> {
        let mut user = match self.users.lock().unwrap().get(username) {
            Some(user) => user.clone(),
            None => return Ok(None),
        };
        if !user.enabled {
            return Err(AuthError::Disabled);
        }
        user.realm = realm.map(|r| r.to_string());
        Ok(Some(user))
    }
}
