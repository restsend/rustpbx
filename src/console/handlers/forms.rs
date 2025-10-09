use serde::Deserialize;

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
