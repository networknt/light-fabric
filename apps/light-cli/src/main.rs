use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use light_cli::config::CliConfig;
use light_cli::error::{CliError, exit};
use light_cli::output::{Output, Stdout};
use light_cli::shell::Shell;
use light_cli::terminal::{self, Terminal};

/// One terminal that stays open until `/exit`: sign in, chat with agents, and (as they arrive)
/// work through tasks and audit trails. Everything is a slash command inside it; text that is not
/// a command goes to the agent you are chatting with.
#[derive(Parser)]
#[command(
    name = "light",
    version,
    after_help = "Non-interactive use: `light -c '/whoami'`, or pipe lines in: `printf '/chat advisor\\nhello\\n' | light`.\n\
                  Exit codes: 0 ok, 1 configuration or internal error, 3 credential refused,\n\
                  5 a service could not be reached (outcome uncertain),\n\
                  6 sign in again (run /login)."
)]
struct Args {
    /// Path to startup.yml.
    #[arg(
        long,
        env = "LIGHT_STARTUP_CONFIG",
        default_value = "config/startup.yml"
    )]
    startup: PathBuf,
    /// Where your login is kept (default ~/.light).
    #[arg(long, env = "LIGHT_HOME")]
    home: Option<PathBuf>,
    /// Run this line as if typed (repeatable) and exit, instead of opening the terminal.
    #[arg(short = 'c', long = "command", value_name = "LINE")]
    command: Vec<String>,
}

async fn run(args: Args) -> Result<(), CliError> {
    let config = CliConfig::load(&args.startup, args.home)
        .inspect_err(|error| eprintln!("error: {error}"))?;
    let history = config.store_dir.join("history");
    let scripted = !args.command.is_empty();
    let at_a_terminal =
        !scripted && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    let mut failure = None;
    if at_a_terminal {
        match Terminal::open(&history) {
            Ok((terminal, out)) => {
                let mut shell = Shell::new(config, out.clone(), true);
                out.line(&shell.banner());
                terminal.run(&mut shell).await;
                failure = shell.take_failure();
            }
            // No usable terminal after all (some CI shells, odd TERM): behave as a pipe.
            Err(reason) => {
                eprintln!("note: no line editor ({reason}); reading plain lines");
                let mut shell = Shell::new(config, Arc::new(Stdout), false);
                terminal::run_stdin(&mut shell).await;
                failure = shell.take_failure();
            }
        }
    } else {
        let mut shell = Shell::new(config, Arc::new(Stdout), false);
        if scripted {
            terminal::run_lines(&mut shell, args.command).await;
        } else {
            terminal::run_stdin(&mut shell).await;
        }
        failure = failure.or(shell.take_failure());
    }
    // A non-interactive run reports whether anything in it failed; the terminal already showed
    // each failure as it happened, and a person leaving it is not a failure.
    match failure {
        Some(error) if !at_a_terminal => Err(error),
        _ => Ok(()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(()) => ExitCode::from(exit::OK),
        // Every error was printed where it happened.
        Err(error) => ExitCode::from(error.exit_code()),
    }
}
