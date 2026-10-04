use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "randdb",
    version,
    about = "Local code indexing and semantic context retrieval for AI agents"
)]
struct Cli {
    /// Configuration and index directory (default: ~/.randdb).
    #[arg(long, global = true, env = "RANDDB_HOME")]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the configuration template without overwriting an existing file.
    Init,
    /// Incrementally index a repository.
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Recompute all embeddings in this index; old ContextWeaver indexes are untouched.
        #[arg(long)]
        force: bool,
    },
    /// Serve the codebase-retrieval tool over MCP stdio.
    Mcp,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "randdb=info,lancedb=warn,lance=warn".into()),
        )
        .init();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("RandDB: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let home = cli.data_dir.unwrap_or_else(randdb::config::default_home);
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    match cli.command {
        Command::Init => {
            let created = randdb::config::initialize(&home)?;
            println!(
                "{} {}",
                if created {
                    "Created"
                } else {
                    "Already exists:"
                },
                home.join(".env").display()
            );
        }
        Command::Index { path, force } => {
            let report = randdb::index(home, path, force, cancel).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Mcp => randdb::mcp::serve(home, cancel).await?,
    }
    Ok(())
}
