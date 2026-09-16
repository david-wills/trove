use std::path::PathBuf;
use std::process::ExitCode;

use rmcp::{transport::stdio, ServiceExt};
use trove_core::Vault;
use trove_mcp::TroveServer;

const USAGE: &str = "\
trove-mcp — read-only MCP server over a Trove vault (stdio transport)

Usage:
  trove-mcp [--vault <path>]

Options:
  --vault <path>   Vault root (default: ~/Documents/Trove; $HOME is honored)
  -h, --help       Show this help

Register with Claude Code:
  claude mcp add trove -- /Applications/Trove.app/Contents/MacOS/trove-mcp
";

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut root: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--vault" => match args.next() {
                Some(p) => root = Some(PathBuf::from(p)),
                None => {
                    eprintln!("--vault needs a path\n\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument: {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let root = root.unwrap_or_else(Vault::default_root);
    if !root.is_dir() {
        eprintln!(
            "trove-mcp: no vault at {} — launch Trove once to create it, or pass --vault <path>",
            root.display()
        );
        return ExitCode::from(2);
    }
    let vault = match Vault::open_or_create(root) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("trove-mcp: cannot open vault: {e:#}");
            return ExitCode::from(1);
        }
    };
    let service = match TroveServer::new(vault).serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("trove-mcp: failed to start: {e}");
            return ExitCode::from(1);
        }
    };
    match service.waiting().await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("trove-mcp: {e}");
            ExitCode::from(1)
        }
    }
}
