//! `iris` CLI entry point. Subcommands: `run`, `chat`, `sessions`, `doctor`
//! (stage 0.2 defines the surface; execution semantics land in stages 1.10+).

mod doctor;
mod gauge;
mod run;
mod sessions;

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
    ///
    /// Exit code = agent outcome: 0 completed · 1 error · 3 max turns ·
    /// 4 token budget exceeded.
    Run {
        /// Prompt to execute
        #[arg(short = 'p', long, value_name = "PROMPT")]
        prompt: String,

        /// Working directory for the agent
        #[arg(long, value_name = "DIR", default_value = ".")]
        workdir: PathBuf,

        /// Model id (OpenRouter slug)
        #[arg(long, value_name = "MODEL", default_value = "openai/gpt-4o-mini")]
        model: String,
    },

    /// Start an interactive chat session
    Chat {
        /// Working directory for the agent
        #[arg(long, value_name = "DIR", default_value = ".")]
        workdir: PathBuf,
    },

    /// Manage saved sessions
    Sessions {
        #[command(subcommand)]
        action: SessionCommands,
    },

    /// Print provider and API key status without exposing secrets
    Doctor,
}

#[derive(Subcommand, Debug)]
enum SessionCommands {
    /// List saved sessions, newest first
    List {
        /// Rebuild the index from the transcripts instead of trusting
        /// the cache (the JSONL remains the source of truth)
        #[arg(long)]
        rebuild: bool,
    },

    /// Resume a session: rehydrate its full history and continue it
    ///
    /// The prior conversation is replayed to the model exactly as it
    /// happened (including repair of a run killed mid-tool-call), then
    /// the new prompt continues it. The transcript keeps growing.
    Resume {
        /// Session id, as shown by `sessions list`
        id: String,

        /// Prompt to continue the session with
        #[arg(short = 'p', long, value_name = "PROMPT")]
        prompt: String,

        /// Working directory (defaults to the session's saved cwd)
        #[arg(long, value_name = "DIR")]
        workdir: Option<PathBuf>,

        /// Model id (defaults to the session's saved model)
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let env: HashMap<String, String> = std::env::vars().collect();

    let code = match &cli.command {
        Commands::Doctor => doctor::run(cli.base_url.as_deref(), &env),
        Commands::Run {
            prompt,
            workdir,
            model,
        } => run::run(prompt, model, workdir, cli.base_url.as_deref(), &env),
        Commands::Chat { workdir } => {
            eprintln!(
                "iris chat is not implemented yet (stage 4.x): workdir={}",
                workdir.display()
            );
            2
        }
        Commands::Sessions { action } => match action {
            SessionCommands::List { rebuild } => sessions::list(*rebuild, &env),
            SessionCommands::Resume {
                id,
                prompt,
                workdir,
                model,
            } => sessions::resume(
                id,
                prompt,
                model.as_deref(),
                workdir.as_deref(),
                cli.base_url.as_deref(),
                &env,
            ),
        },
    };

    std::process::exit(code);
}
