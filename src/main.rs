use clap::{CommandFactory, Parser, Subcommand};
use std::io::Write;
use std::process::ExitCode;
use vault_sync::model::{Command, Failure, Kind};
use vault_sync::{config, control, daemon, interrupt, service};

#[derive(Parser)]
#[command(
    name = "vault-sync",
    version = concat!("v", env!("CARGO_PKG_VERSION")),
    about = "App-independent Git synchronization for a notes vault"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(clap::Args)]
struct Output {
    /// Print machine-readable JSON
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Action {
    /// Watch the vault and poll its remote until stopped
    Daemon,
    /// Wait for a fresh sync; works without a daemon
    Now(Output),
    /// Alias for now
    Sync(Output),
    /// Read live daemon or persisted status without fetching
    Status(Output),
    /// Wait for active work, then persistently pause automation
    Pause(Output),
    /// Resume automation
    Resume(Output),
    /// Install a systemd or launchd definition without starting it
    ServiceInstall,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            // Exit code 2 is reserved for invalid application configuration.
            return ExitCode::from(u8::from(error.use_stderr()));
        }
    };

    if let Err(error) = interrupt::install() {
        eprintln!("Cannot handle signals: {error}");
        return ExitCode::FAILURE;
    }

    let code = match cli.command {
        None => {
            let _ = Cli::command().print_help();
            Ok(0)
        }
        Some(action) => execute(action),
    };

    match code {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            if error.kind != Kind::Interrupted {
                eprintln!("{error}");
            }
            ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(1))
        }
    }
}

fn execute(action: Action) -> Result<u8, Failure> {
    let (command, json) = match action {
        Action::Daemon => {
            return config::load()
                .and_then(|(settings, _)| daemon::run(settings))
                .map(|()| 0);
        }
        Action::ServiceInstall => return service::install().map(|message| print(&message)),
        Action::Now(output) | Action::Sync(output) => (Command::Now, output.json),
        Action::Status(output) => (Command::Status, output.json),
        Action::Pause(output) => (Command::Pause, output.json),
        Action::Resume(output) => (Command::Resume, output.json),
    };

    let (settings, _) = config::load()?;
    let view = control::control(&settings, command)?;

    if json {
        let text = serde_json::to_string_pretty(&view)
            .map_err(|error| Failure::new(Kind::Process, error.to_string()))?;
        print(&text);
    } else {
        print(&control::describe(&view, command));
    }

    Ok(if command == Command::Now {
        u8::try_from(view.state.exit_code()).unwrap_or(1)
    } else {
        0
    })
}

/// Prints a line, ignoring a closed stdout instead of panicking.
fn print(text: &str) -> u8 {
    let _ = writeln!(std::io::stdout().lock(), "{text}");
    0
}
