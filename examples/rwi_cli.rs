// examples/rwi_cli.rs
//
// A simple CLI tool to test RWI commands interactively.
//
// Usage:
//   cargo run --example rwi_cli -- --url ws://127.0.0.1:8088/rwi/v1 --token test-token-123
//
// Commands (send as JSON):
//   {"action": "session.subscribe", "params": {"contexts": ["default"]}}
//   {"action": "call.originate", "params": {"call_id": "test", "destination": "sip:1000@127.0.0.1:5060", "context": "default"}}
//   {"action": "session.list_calls", "params": {}}
//   {"action": "call.hangup", "params": {"call_id": "test"}}
//

use clap::Parser;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::io::{self, Write};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Parser, Debug)]
#[command(name = "rwi_cli")]
#[command(about = "Interactive RWI WebSocket client", long_about = None)]
struct Args {
    /// WebSocket URL (e.g., ws://127.0.0.1:8088/rwi/v1)
    #[arg(long, default_value = "ws://127.0.0.1:8088/rwi/v1")]
    url: String,

    /// RWI token
    #[arg(long, default_value = "test-token-123")]
    token: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Build URL with token
    let url = if args.url.contains('?') {
        format!("{}&token={}", args.url, args.token)
    } else {
        format!("{}?token={}", args.url, args.token)
    };

    println!("Connecting to {}...", url);

    let (ws, _) = connect_async(&url).await?;
    println!("Connected! Type JSON commands (or 'quit' to exit).");
    println!();

    let (mut write, mut read) = ws.split();

    // Spawn task to read events
    let read_task = tokio::spawn(async move {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    let text_str = text.to_string();
                    let v: serde_json::Value = serde_json::from_str(&text_str)
                        .unwrap_or_else(|_| json!({"raw": text_str}));
                    println!("< {}", serde_json::to_string_pretty(&v).unwrap_or(text_str));
                }
                Ok(Message::Close(_)) => {
                    println!("Connection closed by server");
                    break;
                }
                Ok(Message::Ping(_)) => {
                    println!("Received ping");
                }
                Ok(Message::Pong(_)) => {}
                Err(e) => {
                    eprintln!("Error: {}", e);
                    break;
                }
                _ => {}
            }
        }
    });

    // Main loop - read commands from stdin
    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        if input == "quit" || input == "exit" {
            break;
        }

        // Add rwi version and action_id if not present
        let cmd: serde_json::Value = match serde_json::from_str(input) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Invalid JSON: {}", e);
                continue;
            }
        };

        let cmd = if cmd.get("rwi").is_none() || cmd.get("action_id").is_none() {
            let mut cmd = cmd;
            if cmd.get("action").is_some() {
                cmd["rwi"] = json!("1.0");
                if cmd.get("action_id").is_none() {
                    cmd["action_id"] = json!(uuid::Uuid::new_v4().to_string());
                }
            }
            cmd
        } else {
            cmd
        };

        let json_str = serde_json::to_string(&cmd)?;
        println!("> {}", json_str);

        if let Err(e) = write.send(Message::Text(json_str.into())).await {
            eprintln!("Send error: {}", e);
            break;
        }
    }

    write.close().await.ok();
    read_task.abort();

    println!("Goodbye!");
    Ok(())
}
