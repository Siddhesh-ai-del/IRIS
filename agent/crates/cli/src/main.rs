//! `iris` CLI entry point. Subcommands: `run`, `chat`, `sessions`, `doctor`
//! (stage 0.2 defines the surface; execution semantics land in stages 1.10+).

mod doctor;

use std::collections::HashMap;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "iris", version, about = "Terminal-based AI coding agent")]
struct Cli {
    /// Override the provider base URL (highest-precedence config layer)
    #[arg(long, global = true, value_name = "URL")]
    base_url: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a one-shot prompt headlessly
    Run {
        /// Prompt to execute
        #[arg(short = 'p', long, value_name = "PROMPT")]
        prompt: String,

        /// Working directory for the agent
        #[arg(long, value_name = "DIR", default_value = ".")]
        workdir: PathBuf,
    },

    /// Start an interactive chat session
    Chat {
        /// Working directory for the agent
        #[arg(long, value_name = "DIR", default_value = ".")]
        workdir: PathBuf,
    },

    /// Manage saved sessions
    Sessions,

    /// Print provider and API key status without exposing secrets
    Doctor,
}

fn main() {
    let cli = Cli::parse();
    let env: HashMap<String, String> = std::env::vars().collect();

    let code = match &cli.command {
        Commands::Doctor => doctor::run(cli.base_url.as_deref(), &env),
        Commands::Run { prompt, workdir } => {
            eprintln!(
                "iris run is not implemented yet (stage 1.10): prompt={prompt:?}, workdir={}",
                workdir.display()
            );
            2
        }
        Commands::Chat { workdir } => {
            eprintln!(
                "iris chat is not implemented yet (stage 4.x): workdir={}",
                workdir.display()
            );
            2
        }
        Commands::Sessions => {
            eprintln!("iris sessions is not implemented yet (stage 2.3)");
            2
        }
    };

    std::process::exit(code);
}
