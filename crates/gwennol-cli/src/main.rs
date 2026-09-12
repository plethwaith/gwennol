//! The thin binary: parse, pick the mode, install the log, dispatch.
//! Everything else lives in the library so the interactive loop can
//! run in-process under test.

#![forbid(unsafe_code)]

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};
use gwennol::policy::RuleSpec;
use gwennol::{
    Cli, EXIT_USAGE, Fatal, Mode, NO_TERMINAL_NOTICE, choose_mode, frontend, ordered_rule_flags,
    print, tui,
};

fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    // --allow and --deny are one ordered list, which clap keeps only
    // as each flag's positions in the command line.
    let rules = ordered_rule_flags(&matches);

    let mode = choose_mode(
        &cli,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    if mode == Mode::Print && !cli.print {
        eprintln!("{NO_TERMINAL_NOTICE}");
    }

    // The host's log: -v raises the level; RUST_LOG, when set, replaces
    // it with its own directives. Print mode without --log goes to
    // stderr, beside the trace, as today; a session without --log
    // installs no subscriber, so nothing competes with the pane.
    let level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_target(cli.verbose >= 2);
    match (&cli.log, mode) {
        (Some(path), _) => match std::fs::File::create(path) {
            Ok(file) => builder
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .init(),
            Err(e) => {
                eprintln!("gwennol: log {}: {e}", path.display());
                return ExitCode::from(EXIT_USAGE);
            }
        },
        (None, Mode::Print) => builder
            .with_writer(std::io::stderr)
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
            .init(),
        (None, Mode::Interactive) => {}
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("gwennol: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match runtime.block_on(run(cli, rules, mode)) {
        Ok(code) => code,
        Err(Fatal(message)) => {
            eprintln!("gwennol: {message}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Everything after argument parsing: the workspace first, then the mode.
async fn run(cli: Cli, flag_rules: Vec<RuleSpec>, mode: Mode) -> Result<ExitCode, Fatal> {
    let workspace = frontend::workspace(&cli)?;
    match mode {
        Mode::Print => print::run(cli, workspace, flag_rules).await,
        Mode::Interactive => tui::run(cli, workspace, flag_rules).await,
    }
}
