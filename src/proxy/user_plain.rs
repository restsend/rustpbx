use super::user::{SipUser, UserBackend};
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
    pub fn new(path: &String) -> Self {
        info!("Creating PlainTextBackend");
        Self {
            users: Arc::new(Mutex::new(HashMap::new())),
            path: path.clone(),
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
            };
            users.insert(username.to_string(), user);
        }
        info!("Loaded {} users from {}", users.len(), self.path);
        Ok(())
    }
}

#[async_trait]
impl UserBackend for PlainTextBackend {
    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser> {
        let key = if let Some(realm) = realm {
            format!("{}@{}", username, realm)
        } else {
            username.to_string()
        };

        let mut user = match self.users.lock().unwrap().get(&key) {
            Some(user) => user.clone(),
            None => return Err(anyhow::anyhow!("missing user: {}", key)),
        };
        if !user.enabled {
            return Err(anyhow::anyhow!("User is disabled: {}", key));
        }
        user.realm = realm.map(|r| r.to_string());
        Ok(user)
    }
}
