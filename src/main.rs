mod app;
mod install;
mod mcp;

use app::App;
use clap::{Parser, Subcommand, ValueEnum};
use fmem::{memory::Store, protocol, search::SearchMode, Result};
use serde_json::{json, Value};
use std::{fs::File, path::PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Local agent memory for Obsidian, with durable writes and hybrid retrieval"
)]
struct Cli {
    #[arg(long, global = true, env = "FMEM_STATE_DIR")]
    state_dir: Option<PathBuf>,
    #[arg(long, global = true, default_value = ".")]
    project_dir: PathBuf,
    #[arg(long, global = true)]
    read_only: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Lexical,
    Semantic,
    Hybrid,
}
impl From<Mode> for SearchMode {
    fn from(value: Mode) -> Self {
        match value {
            Mode::Lexical => Self::Lexical,
            Mode::Semantic => Self::Semantic,
            Mode::Hybrid => Self::Hybrid,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    Init {
        #[arg(long)]
        vault: PathBuf,
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "auto", value_parser = ["auto", "inbox"])]
        mode: String,
    },
    Status,
    Doctor,
    Search {
        query: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = Mode::Hybrid)]
        mode: Mode,
        #[arg(long, default_value = "project", value_parser = ["project", "shared"])]
        scope: String,
        #[arg(long, value_parser = ["decision", "preference", "correction"])]
        kind: Option<String>,
    },
    Context {
        query: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = Mode::Hybrid)]
        mode: Mode,
        #[arg(long, default_value_t = 12000)]
        max_bytes: usize,
    },
    Read {
        id: String,
        #[arg(long, default_value = "project", value_parser = ["project", "shared"])]
        scope: String,
        #[arg(long)]
        history: Option<usize>,
    },
    Commit {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    Receipt {
        operation_id: String,
    },
    Retry {
        #[arg(long, default_value_t = 64)]
        limit: usize,
    },
    Index {
        #[arg(long)]
        rebuild: bool,
        #[arg(long)]
        semantic: bool,
        #[arg(long)]
        watch: bool,
    },
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    Mcp,
}

#[derive(Subcommand)]
enum ModelCommand {
    Install {
        #[arg(long)]
        from: Option<PathBuf>,
        #[arg(long)]
        runtime: Option<PathBuf>,
    },
}

fn state_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if cfg!(windows) {
        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return Ok(PathBuf::from(path).join("fmem"));
        }
    }
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        if PathBuf::from(&path).is_absolute() {
            return Ok(PathBuf::from(path).join("fmem"));
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or("Set --state-dir or FMEM_STATE_DIR")?;
    Ok(PathBuf::from(home).join(".local/state/fmem"))
}

fn print(value: &Value) {
    println!("{value}");
}

async fn run(cli: Cli) -> Result<()> {
    let store = Store::new(state_dir(cli.state_dir)?)?;
    match cli.command {
        Command::Init {
            vault,
            project,
            mode,
        } => {
            if cli.read_only {
                return Err("init is unavailable in read-only mode".into());
            }
            store.setup(&vault, &cli.project_dir, &project, Some(&mode))?;
            print(&App::new(store, cli.project_dir, false)?.status(None)?);
        }
        Command::Model {
            command: ModelCommand::Install { from, runtime },
        } => {
            if cli.read_only {
                return Err("model install is unavailable in read-only mode".into());
            }
            print(&install::install(
                &store.state,
                from.as_deref(),
                runtime.as_deref(),
            )?);
        }
        Command::Doctor => {
            let binding = store.resolve(&cli.project_dir)?;
            let model = fmem::embeddings::Embeddings::load(&store.state);
            print(&json!({"bound": binding.is_some(), "state": store.state,
                "model_ready": model.is_ok(), "semantic_unavailable": model.err(),
                "version": env!("CARGO_PKG_VERSION")}));
        }
        command => {
            let mut app = App::new(store, cli.project_dir, cli.read_only)?;
            let result = match command {
                Command::Status => app.status(None)?,
                Command::Search {
                    query,
                    limit,
                    mode,
                    scope,
                    kind,
                } => app.search(&query, limit, mode.into(), &scope, kind.as_deref())?,
                Command::Context {
                    query,
                    limit,
                    mode,
                    max_bytes,
                } => app.context(&query, limit, mode.into(), max_bytes)?,
                Command::Read { id, scope, history } => app.read(&id, &scope, history)?,
                Command::Commit { file } => {
                    let request = match file {
                        Some(path) => protocol::read_json(
                            File::open(path).map_err(|e| e.to_string())?,
                            fmem::operations::MAX_REQUEST_BYTES,
                        )?,
                        None => protocol::read_json(
                            std::io::stdin().lock(),
                            fmem::operations::MAX_REQUEST_BYTES,
                        )?,
                    };
                    app.commit(&request)?
                }
                Command::Receipt { operation_id } => app.status(Some(&operation_id))?,
                Command::Retry { limit } => app.retry(limit)?,
                Command::Index {
                    rebuild,
                    semantic,
                    watch,
                } => {
                    let initial = app.index(rebuild, semantic)?;
                    if !watch {
                        initial
                    } else {
                        print(&initial);
                        loop {
                            tokio::select! {
                                result = tokio::signal::ctrl_c() => {
                                    result.map_err(|e| e.to_string())?;
                                    break;
                                }
                                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                                    match app.poll_index(semantic) {
                                        Ok(report) => print(&report),
                                        Err(error) => eprintln!("{error}"),
                                    }
                                }
                            }
                        }
                        return Ok(());
                    }
                }
                Command::Mcp => return mcp::serve(app).await,
                _ => unreachable!(),
            };
            print(&result);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("{}", json!({"error": error}));
        std::process::exit(1);
    }
}
