use agentfs_mcp::{Client, stdio_proxy};
use agentfs_model::{Error, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{io::Read, path::PathBuf};

#[derive(Debug, Parser)]
#[command(version, about = "AgentFS MCP client")]
struct Arguments {
    #[arg(
        long,
        env = "AGENTFS_ENDPOINT",
        default_value = "http://127.0.0.1:7421/mcp"
    )]
    endpoint: String,
    #[arg(long, env = "AGENTFS_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    /// List all tools and their JSON schemas.
    Tools,
    /// Call a tool. Use --request-id for mutations; --json - reads standard input.
    Call {
        tool: String,
        #[arg(long, default_value = "{}")]
        json: String,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Forward MCP over standard input/output to the long-running daemon.
    Mcp,
}
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    match run(Arguments::parse()).await {
        Ok(true) => {}
        Ok(false) => std::process::exit(2),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
async fn run(arguments: Arguments) -> Result<bool> {
    let token = match arguments.token_file {
        Some(path) => std::fs::read_to_string(path)?.trim().to_owned(),
        None => std::env::var("AGENTFS_TOKEN").map_err(|_| {
            Error::invalid("set AGENTFS_TOKEN_FILE, --token-file, or AGENTFS_TOKEN")
        })?,
    };
    let client = Client::connect(&arguments.endpoint, token).await?;
    match arguments.command {
        Command::Tools => println!("{}", serde_json::to_string_pretty(&client.tools().await?)?),
        Command::Call {
            tool,
            json: input,
            request_id,
        } => {
            let input = if input == "-" {
                let mut input = String::new();
                std::io::stdin()
                    .take(8 * 1024 * 1024 + 1)
                    .read_to_string(&mut input)?;
                input
            } else {
                input
            };
            if input.len() > 8 * 1024 * 1024 {
                return Err(Error::invalid("tool arguments exceed 8 MiB"));
            }
            let mut arguments: Value =
                serde_json::from_str(&input).map_err(|error| Error::invalid(error.to_string()))?;
            if let Some(request_id) = request_id {
                arguments = json!({"request_id": request_id, "request": arguments});
            }
            let result = client.call(tool, arguments).await?;
            let success = result.is_error != Some(true);
            println!(
                "{}",
                serde_json::to_string_pretty(&result.structured_content.unwrap_or_else(
                    || json!({"content": result.content, "is_error": result.is_error})
                ))?
            );
            return Ok(success);
        }
        Command::Mcp => stdio_proxy(client).await?,
    }
    Ok(true)
}
