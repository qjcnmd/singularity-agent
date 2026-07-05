use clap::{Parser, Subcommand};
use serde_json::json;
use singularity_core::ClientInfo;
use singularity_protocol::{InitializeParams, JsonRpcMessage, Method};

#[derive(Debug, Parser)]
#[command(name = "singularity-rs")]
#[command(about = "Rust CLI client for the Singularity app-server protocol")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the JSON-RPC initialize request used by the protocol client.
    ProtocolInit,
    /// Print a thread/start JSON-RPC request.
    ThreadStart {
        #[arg(long)]
        model: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::ProtocolInit) {
        Command::ProtocolInit => {
            let request = JsonRpcMessage::request(
                Method::Initialize,
                json!(0),
                serde_json::to_value(InitializeParams {
                    client_info: ClientInfo::new(
                        "singularity_cli",
                        "Singularity Rust CLI",
                        "0.1.0",
                    ),
                    capabilities: None,
                })
                .expect("serialize initialize params"),
            );
            println!("{}", request.to_wire_value());
        }
        Command::ThreadStart { model } => {
            let request =
                JsonRpcMessage::request(Method::ThreadStart, json!(1), json!({"model": model}));
            println!("{}", request.to_wire_value());
        }
    }
}
