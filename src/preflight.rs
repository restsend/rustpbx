use crate::{app::AppState, config::Config};
use serde::Serialize;
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
};
use tokio::net::{TcpListener, UdpSocket};

#[derive(Debug, Serialize)]
pub struct PreflightIssue {
    pub field: String,
    pub message: String,
}

#[derive(Debug)]
pub struct PreflightError {
    pub issues: Vec<PreflightIssue>,
}

impl PreflightError {
    pub fn new(issues: Vec<PreflightIssue>) -> Self {
        Self { issues }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SocketKind {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PortKey {
    kind: SocketKind,
    port: u16,
}

struct BindTarget {
    field: String,
    addr: SocketAddr,
    kind: SocketKind,
}

pub async fn validate_reload(state: &AppState, proposed: &Config) -> Result<(), PreflightError> {
    let mut issues = Vec::new();

    #[cfg(feature = "console")]
    {
        let console_running = state.console.is_some() || state.config().console.is_some();
        if console_running && proposed.console.is_none() {
            issues.push(PreflightIssue {
                field: "console".to_string(),
                message: "Reload would disable the console (`console` section missing). Please keep it enabled while reloading from the UI.".to_string(),
            });
        }
    }

    let current_config = state.config().as_ref();
    let current_ports = current_port_keys(current_config);

    let (targets, mut parse_issues) = bind_targets(proposed);
    issues.append(&mut parse_issues);

    issues.append(&mut check_bind_targets(targets, &current_ports).await);

    if issues.is_empty() {
        Ok(())
    } else {
        Err(PreflightError::new(issues))
    }
}

pub async fn validate_start(config: &Config) -> Result<(), PreflightError> {
    let mut issues = Vec::new();

    let (targets, mut parse_issues) = bind_targets(config);
    issues.append(&mut parse_issues);

    let current_ports = HashSet::new();
    issues.append(&mut check_bind_targets(targets, &current_ports).await);

    if issues.is_empty() {
        Ok(())
    } else {
        Err(PreflightError::new(issues))
    }
}

async fn check_bind_targets(
    targets: Vec<BindTarget>,
    skip_ports: &HashSet<PortKey>,
) -> Vec<PreflightIssue> {
    let mut issues = Vec::new();
    let mut tested = HashSet::new();

    for target in targets {
        let key = PortKey {
            kind: target.kind,
            port: target.addr.port(),
        };

        if skip_ports.contains(&key) {
            continue;
        }

        if !tested.insert((target.kind, target.addr)) {
            continue;
        }

        let result = match target.kind {
            SocketKind::Tcp => TcpListener::bind(target.addr)
                .await
                .map(|listener| drop(listener)),
            SocketKind::Udp => UdpSocket::bind(target.addr)
                .await
                .map(|socket| drop(socket)),
        };

        if let Err(err) = result {
            issues.push(PreflightIssue {
                field: target.field,
                message: format!("Address {} is unavailable ({})", target.addr, err),
            });
        }
    }

    issues
}

fn current_port_keys(config: &Config) -> HashSet<PortKey> {
    let mut set = HashSet::new();

    if let Ok(addr) = value_as_socket_addr("http_addr", &config.http_addr) {
        set.insert(PortKey {
            kind: SocketKind::Tcp,
            port: addr.port(),
        });
    }

    insert_optional_port(&mut set, SocketKind::Udp, config.proxy.udp_port);
    insert_optional_port(&mut set, SocketKind::Tcp, config.proxy.tcp_port);
    insert_optional_port(&mut set, SocketKind::Tcp, config.proxy.tls_port);
    insert_optional_port(&mut set, SocketKind::Tcp, config.proxy.ws_port);
    set
}

fn insert_optional_port(target: &mut HashSet<PortKey>, kind: SocketKind, port: Option<u16>) {
    if let Some(port) = port {
        target.insert(PortKey { kind, port });
    }
}

fn bind_targets(config: &Config) -> (Vec<BindTarget>, Vec<PreflightIssue>) {
    let mut targets = Vec::new();
    let mut issues = Vec::new();

    match value_as_socket_addr("http_addr", &config.http_addr) {
        Ok(addr) => targets.push(BindTarget {
            field: "http_addr".to_string(),
            addr,
            kind: SocketKind::Tcp,
        }),
        Err(issue) => issues.push(issue),
    }

    match value_as_ip_addr("proxy.addr", &config.proxy.addr) {
        Ok(ip) => {
            push_target(
                &mut targets,
                "proxy.udp_port",
                ip,
                config.proxy.udp_port,
                SocketKind::Udp,
            );
            push_target(
                &mut targets,
                "proxy.tcp_port",
                ip,
                config.proxy.tcp_port,
                SocketKind::Tcp,
            );
            push_target(
                &mut targets,
                "proxy.tls_port",
                ip,
                config.proxy.tls_port,
                SocketKind::Tcp,
            );
            push_target(
                &mut targets,
                "proxy.ws_port",
                ip,
                config.proxy.ws_port,
                SocketKind::Tcp,
            );
        }
        Err(issue) => issues.push(issue),
    }

    (targets, issues)
}

fn push_target(
    targets: &mut Vec<BindTarget>,
    field: &str,
    ip: IpAddr,
    port: Option<u16>,
    kind: SocketKind,
) {
    if let Some(port) = port {
        targets.push(BindTarget {
            field: field.to_string(),
            addr: SocketAddr::new(ip, port),
            kind,
        });
    }
}

fn value_as_socket_addr(field: &str, value: &str) -> Result<SocketAddr, PreflightIssue> {
    value.parse::<SocketAddr>().map_err(|err| PreflightIssue {
        field: field.to_string(),
        message: format!("Invalid {} `{}` ({})", field, value, err),
    })
}

fn value_as_ip_addr(field: &str, value: &str) -> Result<IpAddr, PreflightIssue> {
    value.parse::<IpAddr>().map_err(|err| PreflightIssue {
        field: field.to_string(),
        message: format!("Invalid {} `{}` ({})", field, value, err),
    })
}
