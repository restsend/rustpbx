use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    author,
    version = crate::version::get_short_version(),
    about = "A versatile SIP PBX server implemented in Rust",
    long_about = crate::version::get_version_info()
)]
pub struct Cli {
    /// Path to the configuration file
    #[clap(
        long,
        global = true,
        help = "Path to the configuration file (TOML format)"
    )]
    pub conf: Option<String>,
    #[clap(
        long,
        global = true,
        help = "Tokio console server address, e.g. /tmp/tokio-console or 127.0.0.1:5556"
    )]
    pub tokio_console: Option<String>,
    #[cfg(feature = "console")]
    #[clap(
        long,
        global = true,
        requires = "super_password",
        help = "Create or update a console super user before starting the server"
    )]
    pub super_username: Option<String>,
    #[cfg(feature = "console")]
    #[clap(
        long,
        global = true,
        requires = "super_username",
        help = "Password for the console super user"
    )]
    pub super_password: Option<String>,
    #[cfg(feature = "console")]
    #[clap(
        long,
        global = true,
        requires = "super_username",
        help = "Email for the console super user (defaults to username@localhost)"
    )]
    pub super_email: Option<String>,
    #[clap(
        long,
        global = true,
        help = "Skip running database migrations on startup"
    )]
    pub skip_migrate: bool,
    #[clap(long, global = true, help = "Run as a rustpbx-master worker")]
    pub master: Option<String>,
    #[clap(long, global = true, default_value = "0")]
    pub worker_gen: u32,
    /// RTP port slice start assigned by the master (overrides config).
    #[clap(long, global = true)]
    pub rtp_start: Option<u16>,
    /// RTP port slice end assigned by the master (overrides config).
    #[clap(long, global = true)]
    pub rtp_end: Option<u16>,
    #[clap(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Validate configuration and exit without starting the server
    CheckConfig,
    /// Dump database DDL schema to stdout
    Dump {
        /// Database URL (overrides the config's database_url)
        #[clap(short, long)]
        database_url: Option<String>,
    },
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            conf: None,
            tokio_console: None,
            #[cfg(feature = "console")]
            super_username: None,
            #[cfg(feature = "console")]
            super_password: None,
            #[cfg(feature = "console")]
            super_email: None,
            skip_migrate: false,
            master: None,
            worker_gen: 0,
            rtp_start: None,
            rtp_end: None,
            command: None,
        }
    }
}
