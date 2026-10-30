use clap::{Parser, Subcommand};
use rustpbx_master::admin::admin_sock_path;
use rustpbx_master::config::ConfigSubset;
use rustpbx_master::run::{self, MasterOpts};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(author, version, about = "rustpbx-master")]
struct Cli {
    #[clap(long, default_value = "config.toml")]
    conf: PathBuf,
    #[clap(long, default_value = "/tmp/rustpbx-master.sock")]
    master_sock: PathBuf,
    /// Override the admin socket path (defaults to `admin.sock` next to the
    /// worker IPC socket, or `[master].admin_sock` from the config).
    #[clap(long)]
    admin_sock: Option<PathBuf>,
    #[clap(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Show the running master's generation status
    Status,
    /// Statically validate a staged release (default: newest)
    Test {
        #[clap(long)]
        release: Option<String>,
    },
    /// Rotate to a staged release (default: newest); same binary = config/.env reload
    Reload {
        #[clap(long)]
        release: Option<String>,
    },
}

/// Resolve the admin socket path for client commands: explicit override wins,
/// else `[master].admin_sock` from the config, else `admin.sock` next to the
/// worker IPC socket.
fn client_sock(cli: &Cli) -> PathBuf {
    let configured = ConfigSubset::load(&cli.conf)
        .ok()
        .and_then(|c| c.master.admin_sock);
    cli.admin_sock
        .clone()
        .or(configured.map(PathBuf::from))
        .unwrap_or_else(|| admin_sock_path(&cli.master_sock, None))
}

async fn run_client(cli: &Cli, cmd: &Commands) -> Result<(), String> {
    let sock = client_sock(cli);
    let (name, release) = match cmd {
        Commands::Status => ("status", None),
        Commands::Test { release } => ("test", release.as_deref()),
        Commands::Reload { release } => ("reload", release.as_deref()),
    };
    let resp = rustpbx_master::admin_client::call(&sock, name, release).await?;
    println!("{resp}");
    match resp.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => Ok(()),
        _ => Err("command failed".into()),
    }
}

fn main() {
    let cli = Cli::parse();

    // Client commands: talk to a running master and exit.
    if let Some(cmd) = &cli.command {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client runtime");
        let result = rt.block_on(run_client(&cli, cmd));
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }

    // Daemon: own the public listeners and supervise worker generations.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    if let Err(e) = rt.block_on(run::run(MasterOpts {
        conf: cli.conf,
        master_sock: cli.master_sock,
    })) {
        eprintln!("rustpbx-master: fatal: {e:#}");
        std::process::exit(1);
    }
}
