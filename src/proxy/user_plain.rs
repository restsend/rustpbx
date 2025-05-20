use super::user::{SipUser, UserBackend};
use anyhow::Result;
use async_trait::async_trait;
use std::io::BufRead;
use std::sync::{Arc, Mutex};
use std::{collections::HashMap, fs::File, io, path::Path};

pub struct PlainTextBackend {
    users: Arc<Mutex<HashMap<String, SipUser>>>,
    path: String,
}

impl PlainTextBackend {
    pub fn new(path: &String) -> Self {
        Self {
            users: Arc::new(Mutex::new(HashMap::new())),
            path: path.clone(),
        }
    }

    pub async fn load(&self) -> Result<()> {
        let path = Path::new(&self.path);
        let file = File::open(path)?;
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
            };
            users.insert(username.to_string(), user);
        }

        Ok(())
    }
}

#[async_trait]
impl UserBackend for PlainTextBackend {
    async fn authenticate(&self, username: &str, password: &str) -> Result<bool> {
        if let Some(user) = self.users.lock().unwrap().get(username) {
            Ok(user.password == Some(password.to_string()))
        } else {
            Ok(false)
        }
    }

    async fn get_user(&self, username: &str, realm: Option<&str>) -> Result<SipUser> {
        let key = if let Some(realm) = realm {
            format!("{}@{}", username, realm)
        } else {
            username.to_string()
        };

        self.users
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("User not found"))
    }
}
