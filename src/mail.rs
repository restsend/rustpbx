use async_trait::async_trait;
use crate::branding::BrandingProvider;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn default_smtp_port() -> u16 {
    25
}

fn default_mail_timeout() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MailConfig {
    #[serde(default)]
    pub enabled: bool,
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    pub from: String,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default = "default_mail_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct MailAttachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct OutboundMail {
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
    pub from_name: Option<String>,
    pub footer: Option<String>,
    pub attachments: Vec<MailAttachment>,
}

fn base64_wrapped(data: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 + 2);
    for (index, chunk) in encoded.as_bytes().chunks(76).enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    out.push_str("\r\n");
    out
}

#[derive(Debug, Clone)]
pub struct EmailTemplate {
    pub subject: String,
    pub body: String,
}

pub fn render_template(template: &str, vars: &[(&str, &str)]) -> String {
    if template.is_empty() {
        return String::new();
    }
    let mut rendered = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut cursor = 0;
    while cursor < template.len() {
        if bytes[cursor] == b'{' && cursor + 1 < template.len() && bytes[cursor + 1] == b'{' {
            if let Some(offset) = template[cursor + 2..].find("}}") {
                let name_start = cursor + 2;
                let name_end = name_start + offset;
                let name = template[name_start..name_end].trim();
                match vars.iter().find(|(key, _)| *key == name) {
                    Some((_, value)) => rendered.push_str(value),
                    None => rendered.push_str(&template[cursor..name_end + 2]),
                }
                cursor = name_end + 2;
                continue;
            }
        }
        let ch = template[cursor..].chars().next().unwrap();
        rendered.push(ch);
        cursor += ch.len_utf8();
    }
    rendered
}

#[async_trait]
pub trait MailTransport: Send + Sync {
    async fn send(&self, mail: &OutboundMail) -> anyhow::Result<()>;
}

pub struct NoopTransport;

#[async_trait]
impl MailTransport for NoopTransport {
    async fn send(&self, _mail: &OutboundMail) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct SmtpTransport {
    config: MailConfig,
}

impl SmtpTransport {
    pub fn new(config: MailConfig) -> Self {
        Self { config }
    }
}

fn encode_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

fn format_from(config: &MailConfig, from_name: Option<&str>) -> String {
    let addr = encode_header(&config.from);
    match from_name {
        Some(name) => {
            let name = encode_header(name);
            let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{}\" <{}>", escaped, addr)
        }
        None => addr,
    }
}

fn render_body(body: &str, footer: Option<&str>) -> String {
    let mut rendered = body.to_string();
    if let Some(footer) = footer.filter(|f| !f.is_empty()) {
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str(footer);
    }
    rendered
}

fn header_block(config: &MailConfig, mail: &OutboundMail, recipients: &[String]) -> String {
    let mut payload = String::new();
    payload.push_str(&format!(
        "From: {}\r\n",
        format_from(config, mail.from_name.as_deref())
    ));
    payload.push_str(&format!(
        "To: {}\r\n",
        recipients
            .iter()
            .map(|r| encode_header(r))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    payload.push_str(&format!("Subject: {}\r\n", encode_header(&mail.subject)));
    payload.push_str("MIME-Version: 1.0\r\n");
    payload
}

fn dot_stuff_and_terminate(payload: &str) -> String {
    let mut out = String::new();
    for line in payload.split('\n') {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line.trim_end_matches('\r'));
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

fn random_boundary(rng: &ring::rand::SystemRandom, extra_bytes: usize) -> String {
    let random: [u8; 32] = ring::rand::generate(rng)
        .map(|random| random.expose())
        .unwrap_or([0u8; 32]);
    let hex: String = random[..24 + extra_bytes]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("=_rustpbx_{hex}")
}

fn multipart_content_contains(mail: &OutboundMail, boundary: &str) -> bool {
    if render_body(&mail.body, mail.footer.as_deref()).contains(boundary) {
        return true;
    }
    mail.attachments
        .iter()
        .any(|attachment| base64_wrapped(&attachment.data).contains(boundary))
}

pub fn build_payload(config: &MailConfig, mail: &OutboundMail, recipients: &[String]) -> String {
    let body = render_body(&mail.body, mail.footer.as_deref());
    if mail.attachments.is_empty() {
        let mut payload = header_block(config, mail, recipients);
        payload.push_str("Content-Type: text/plain; charset=utf-8\r\n");
        payload.push_str("\r\n");
        payload.push_str(&body);
        dot_stuff_and_terminate(&payload)
    } else {
        let rng = ring::rand::SystemRandom::new();
        let mut extra_bytes = 0usize;
        let mut boundary = random_boundary(&rng, extra_bytes);
        while extra_bytes < 8 && multipart_content_contains(mail, &boundary) {
            extra_bytes += 1;
            boundary = random_boundary(&rng, extra_bytes);
        }
        build_multipart_payload(config, mail, recipients, &boundary)
    }
}

fn build_multipart_payload(
    config: &MailConfig,
    mail: &OutboundMail,
    recipients: &[String],
    boundary: &str,
) -> String {
    let body = render_body(&mail.body, mail.footer.as_deref());
    let mut payload = header_block(config, mail, recipients);
    payload.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n"
    ));
    payload.push_str("\r\n");
    payload.push_str(&format!("--{boundary}\r\n"));
    payload.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    payload.push_str("Content-Transfer-Encoding: 8bit\r\n");
    payload.push_str("\r\n");
    payload.push_str(&body);
    payload.push_str("\r\n");
    for attachment in &mail.attachments {
        let filename = encode_header(&attachment.filename);
        payload.push_str(&format!("--{boundary}\r\n"));
        payload.push_str(&format!(
            "Content-Type: {}; name=\"{}\"\r\n",
            attachment.content_type, filename
        ));
        payload.push_str("Content-Transfer-Encoding: base64\r\n");
        payload.push_str(&format!(
            "Content-Disposition: attachment; filename=\"{filename}\"\r\n"
        ));
        payload.push_str("\r\n");
        payload.push_str(&base64_wrapped(&attachment.data));
        payload.push_str("\r\n");
    }
    payload.push_str(&format!("--{boundary}--\r\n"));
    dot_stuff_and_terminate(&payload)
}

pub fn apply_brand(mail: &mut OutboundMail, provider: Option<&Arc<dyn BrandingProvider>>) {
    if let Some(provider) = provider {
        mail.from_name = provider.email_from_name();
        mail.footer = provider.email_footer();
    }
}

#[async_trait]
impl MailTransport for SmtpTransport {
    async fn send(&self, mail: &OutboundMail) -> anyhow::Result<()> {
        let cfg = &self.config;
        let timeout = std::time::Duration::from_secs(cfg.timeout_secs.max(1));
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let stream = tokio::time::timeout(timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| anyhow::anyhow!("smtp connect timed out: {addr}"))??;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        async fn expect(
            reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
            prefixes: &[&str],
        ) -> anyhow::Result<()> {
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            if prefixes.iter().any(|p| line.starts_with(p)) {
                Ok(())
            } else {
                Err(anyhow::anyhow!("unexpected smtp reply: {}", line.trim()))
            }
        }

        async fn cmd(
            reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
            writer: &mut tokio::net::tcp::OwnedWriteHalf,
            command: &str,
            expect_prefix: &str,
        ) -> anyhow::Result<()> {
            writer.write_all(command.as_bytes()).await?;
            writer.write_all(b"\r\n").await?;
            writer.flush().await?;
            expect(reader, &[expect_prefix]).await
        }

        expect(&mut reader, &["220"]).await?;
        cmd(&mut reader, &mut writer, "EHLO rustpbx", "250").await?;

        if let (Some(user), Some(pass)) = (cfg.username.as_ref(), cfg.password.as_ref()) {
            cmd(&mut reader, &mut writer, "AUTH LOGIN", "334").await?;
            let user_b64 = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                user.as_bytes(),
            );
            cmd(&mut reader, &mut writer, &user_b64, "334").await?;
            let pass_b64 = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                pass.as_bytes(),
            );
            cmd(&mut reader, &mut writer, &pass_b64, "235").await?;
        }

        cmd(
            &mut reader,
            &mut writer,
            &format!("MAIL FROM:<{}>", encode_header(&cfg.from)),
            "250",
        )
        .await?;

        let recipients: Vec<String> = if mail.to.is_empty() {
            cfg.to.clone()
        } else {
            mail.to.clone()
        };
        if recipients.is_empty() {
            return Err(anyhow::anyhow!("no mail recipients configured"));
        }
        for rcpt in &recipients {
            cmd(
                &mut reader,
                &mut writer,
                &format!("RCPT TO:<{}>", encode_header(rcpt)),
                "250",
            )
            .await?;
        }

        cmd(&mut reader, &mut writer, "DATA", "354").await?;
        let payload = build_payload(cfg, mail, &recipients);
        writer.write_all(payload.as_bytes()).await?;
        writer.flush().await?;
        expect(&mut reader, &["250"]).await?;

        let _ = cmd(&mut reader, &mut writer, "QUIT", "221").await;
        Ok(())
    }
}

pub fn transport_from_config(config: Option<&MailConfig>) -> Option<Arc<dyn MailTransport>> {
    let config = config?;
    if !config.enabled || config.to.is_empty() {
        return None;
    }
    Some(Arc::new(SmtpTransport::new(config.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_or_recipientless_config_yields_no_transport() {
        let mut config = MailConfig {
            enabled: false,
            host: "localhost".into(),
            port: 25,
            username: None,
            password: None,
            from: "pbx@example.com".into(),
            to: vec!["ops@example.com".into()],
            timeout_secs: 10,
        };
        assert!(transport_from_config(Some(&config)).is_none());
        config.enabled = true;
        config.to.clear();
        assert!(transport_from_config(Some(&config)).is_none());
        config.to.push("ops@example.com".into());
        assert!(transport_from_config(Some(&config)).is_some());
        assert!(transport_from_config(None).is_none());
    }

    #[test]
    fn header_values_are_sanitized() {
        assert_eq!(encode_header("a\r\nb\r\n"), "a  b  ");
    }

    fn test_config(from: &str) -> MailConfig {
        MailConfig {
            enabled: true,
            host: "localhost".into(),
            port: 25,
            username: None,
            password: None,
            from: from.into(),
            to: vec![],
            timeout_secs: 10,
        }
    }

    fn test_mail(body: &str) -> OutboundMail {
        OutboundMail {
            to: vec![],
            subject: "Subject".into(),
            body: body.into(),
            from_name: None,
            footer: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn from_name_is_quoted_and_sanitized() {
        let config = test_config("pbx@example.com");
        let mut mail = test_mail("hello");
        mail.from_name = Some("Acme \"PBX\"\r\nBcc: evil".into());
        let payload = build_payload(&config, &mail, &["ops@example.com".into()]);
        assert!(payload.starts_with(
            "From: \"Acme \\\"PBX\\\"  Bcc: evil\" <pbx@example.com>\r\n"
        ));
        assert!(!payload.contains("\r\nBcc: evil"));
    }

    #[test]
    fn missing_from_name_leaves_from_header_unchanged() {
        let config = test_config("pbx@example.com");
        let mail = test_mail("hello");
        let payload = build_payload(&config, &mail, &["ops@example.com".into()]);
        assert!(payload.starts_with("From: pbx@example.com\r\n"));
    }

    #[test]
    fn footer_is_appended_to_the_body() {
        let config = test_config("pbx@example.com");
        let mut mail = test_mail("hello");
        mail.footer = Some("Regards\nAcme PBX".into());
        let payload = build_payload(&config, &mail, &["ops@example.com".into()]);
        assert!(payload.contains("\r\n\r\nhello\r\n\r\nRegards\r\nAcme PBX\r\n.\r\n"));
    }

    #[test]
    fn empty_footer_leaves_body_unchanged() {
        let config = test_config("pbx@example.com");
        let mut mail = test_mail("hello");
        mail.footer = Some(String::new());
        let with_empty = build_payload(&config, &mail, &["ops@example.com".into()]);
        mail.footer = None;
        let with_none = build_payload(&config, &mail, &["ops@example.com".into()]);
        assert_eq!(with_empty, with_none);
    }

    #[test]
    fn dot_stuffing_is_preserved() {
        let config = test_config("pbx@example.com");
        let mail = test_mail("line1\n.leading\n..double");
        let payload = build_payload(&config, &mail, &["ops@example.com".into()]);
        assert!(payload.contains("\r\nline1\r\n..leading\r\n...double\r\n.\r\n"));
    }

    #[test]
    fn render_template_replaces_placeholders() {
        let rendered = render_template(
            "rustpbx {{alert_type}}: {{ip}}",
            &[("alert_type", "auth_ban"), ("ip", "1.2.3.4")],
        );
        assert_eq!(rendered, "rustpbx auth_ban: 1.2.3.4");
    }

    #[test]
    fn render_template_allows_whitespace_inside_braces() {
        let rendered = render_template("{{ alert_type }}|{{ip }}|{{  ip  }}", &[
            ("alert_type", "auth_ban"),
            ("ip", "1.2.3.4"),
        ]);
        assert_eq!(rendered, "auth_ban|1.2.3.4|1.2.3.4");
    }

    #[test]
    fn render_template_keeps_unknown_placeholders_verbatim() {
        let rendered = render_template("{{known}} {{missing}} {{bad", &[("known", "yes")]);
        assert_eq!(rendered, "yes {{missing}} {{bad");
    }

    #[test]
    fn render_template_replaces_repeated_placeholders() {
        let rendered = render_template("{{ip}}/{{ip}}/{{ip}}", &[("ip", "9.9.9.9")]);
        assert_eq!(rendered, "9.9.9.9/9.9.9.9/9.9.9.9");
    }

    #[test]
    fn render_template_empty_input_yields_empty_output() {
        assert_eq!(render_template("", &[("ip", "9.9.9.9")]), "");
    }

    fn attachment_config() -> MailConfig {
        MailConfig {
            enabled: true,
            host: "localhost".into(),
            port: 25,
            username: None,
            password: None,
            from: "pbx@example.com".into(),
            to: vec!["ops@example.com".into()],
            timeout_secs: 10,
        }
    }

    #[test]
    fn base64_wrapped_round_trips_and_wraps_at_76() {
        use base64::Engine as _;
        let data: Vec<u8> = (0..200u16).map(|b| b as u8).collect();
        let wrapped = base64_wrapped(&data);
        for line in wrapped.trim_end_matches("\r\n").split("\r\n") {
            assert!(line.len() <= 76);
        }
        let joined: String = wrapped.split("\r\n").collect();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(joined.as_bytes())
            .expect("decode");
        assert_eq!(decoded, data);
    }

    #[test]
    fn build_payload_without_attachments_stays_text() {
        let payload = build_payload(&attachment_config(), &test_mail("hello"), &["ops@example.com".into()]);
        assert!(payload.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(!payload.contains("multipart/mixed"));
    }

    fn multipart_boundary(payload: &str) -> String {
        let marker = "Content-Type: multipart/mixed; boundary=\"";
        let start = payload.find(marker).expect("multipart header present") + marker.len();
        let remainder = &payload[start..];
        let end = remainder.find('"').expect("boundary closing quote");
        remainder[..end].to_string()
    }

    #[test]
    fn build_payload_attaches_files_as_multipart() {
        let data = b"%PDF-1.4 fake fax".to_vec();
        let mut mail = test_mail("see attachment");
        mail.attachments.push(MailAttachment {
            filename: "fax-7.pdf".into(),
            content_type: "application/pdf".into(),
            data: data.clone(),
        });
        let payload = build_payload(&attachment_config(), &mail, &["ops@example.com".into()]);
        let boundary = multipart_boundary(&payload);
        assert!(boundary.starts_with("=_rustpbx_"));
        assert!(payload.contains(&format!(
            "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n"
        )));
        assert!(payload.contains(&format!("--{boundary}\r\n")));
        assert!(payload.contains("Content-Type: application/pdf; name=\"fax-7.pdf\"\r\n"));
        assert!(payload.contains("Content-Transfer-Encoding: base64\r\n"));
        assert!(payload.contains("Content-Disposition: attachment; filename=\"fax-7.pdf\"\r\n"));
        assert!(payload.contains(&format!("--{boundary}--\r\n")));
        assert!(payload.contains(&base64_wrapped(&data).trim_end_matches("\r\n")));
        assert!(payload.ends_with(".\r\n"));
    }

    #[test]
    fn build_payload_supports_multiple_attachments() {
        let mut mail = test_mail("two");
        for name in ["a.pdf", "b.tiff"] {
            mail.attachments.push(MailAttachment {
                filename: name.into(),
                content_type: "application/octet-stream".into(),
                data: vec![1, 2, 3],
            });
        }
        let payload = build_payload(&attachment_config(), &mail, &["ops@example.com".into()]);
        let boundary = multipart_boundary(&payload);
        assert_eq!(payload.matches(&format!("--{boundary}\r\n")).count(), 3);
        assert!(payload.contains("filename=\"a.pdf\""));
        assert!(payload.contains("filename=\"b.tiff\""));
    }

    #[test]
    fn build_payload_generates_unique_boundaries_per_message() {
        let mut mail = test_mail("hello");
        mail.attachments.push(MailAttachment {
            filename: "a.pdf".into(),
            content_type: "application/pdf".into(),
            data: vec![1, 2, 3],
        });
        let first = build_payload(&attachment_config(), &mail, &["ops@example.com".into()]);
        let second = build_payload(&attachment_config(), &mail, &["ops@example.com".into()]);
        assert_ne!(multipart_boundary(&first), multipart_boundary(&second));
    }

    #[test]
    fn multipart_content_detection_flags_boundary_in_body() {
        let mail = test_mail("before =_rustpbx_c0ffee after");
        assert!(multipart_content_contains(&mail, "=_rustpbx_c0ffee"));
        assert!(!multipart_content_contains(&mail, "=_rustpbx_deadbeef"));
    }
}
