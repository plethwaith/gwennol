//! Gwennol command-line frontend.
//!
//! Two `Operator` implementations share one binary: an interactive
//! session, by default, and `-p`/`--print`, one task with the model's
//! text on stdout and the trace on stderr, no input. Both decide every
//! approval by a rule and trace it — a session into its transcript
//! pane, a print run to stderr — and both go through [`frontend::start`]
//! and share [`show`]'s words, so a session reads like a print run's
//! stderr. `-p`, or a run with no terminal on stdin or stdout, is print
//! mode; otherwise a session opens.
//!
//! Exit status: 0 when the turn completed or the user ended the
//! session; 1 when the turn failed, or the session ended right after a
//! failed turn; 2 for a usage, configuration or startup error; 130 when
//! cancelled by Ctrl-C in print mode, or forced by a second `/exit`.

#![forbid(unsafe_code)]

pub mod config;
pub mod frontend;
pub mod operator;
pub mod plugins;
pub mod policy;
pub mod print;
pub mod secrets;
pub mod show;
pub mod tui;

use std::path::PathBuf;

use clap::{ArgAction, Parser};
use gwennol_core::Decision;

use policy::{RuleSpec, Source};

/// An interactive session, or one task with -p; every approval decided
/// by a rule.
#[derive(Debug, Parser)]
#[command(name = "gwennol", version, about, long_about = None)]
pub struct Cli {
    /// The first turn of the session, or the task in print mode.
    /// Absent or `-`: print mode reads it from stdin; a session
    /// starts with an empty editor.
    pub prompt: Option<String>,

    /// The workspace root: where relative paths resolve and commands
    /// run. Default: the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,

    /// The config file. Default: $XDG_CONFIG_HOME/gwennol/config.toml,
    /// when it exists.
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// A rules-only file, tried after the flags and before the config
    /// file's rules.
    #[arg(
        long,
        value_name = "FILE",
        help = "A file of [[rules]], tried after the flags and before the config file's rules"
    )]
    pub policy: Option<PathBuf>,

    /// Allow requests matching a rule (see `policy`). Repeatable; tried
    /// in the order given together with `deny`.
    #[arg(
        long,
        value_name = "RULE",
        action = ArgAction::Append,
        help = "Allow requests matching RULE (<kind>:<glob>; kinds read, write, list, spawn, http, any). \
                Repeatable; tried in the order given together with --deny. Nothing matching any rule is denied"
    )]
    pub allow: Vec<String>,

    /// Deny requests matching RULE. Repeatable; see --allow.
    #[arg(long, value_name = "RULE", action = ArgAction::Append)]
    pub deny: Vec<String>,

    /// The directory of bundled plugin manifests (`cargo xtask bundle`
    /// writes target/bundle/plugins).
    #[arg(long, value_name = "DIR", env = "GWENNOL_PLUGINS")]
    pub plugins: Option<PathBuf>,

    /// Trust PLUGIN to supply a script runtime. Repeatable; adds to
    /// the config file's list.
    #[arg(long = "trust-runtime", value_name = "PLUGIN")]
    pub trust_runtime: Vec<String>,

    /// Where a plugin's secret comes from (see `secrets`). Repeatable;
    /// flags are tried before the config file's entries.
    #[arg(
        long,
        value_name = "PLUGIN:NAME=SOURCE",
        help = "Where a plugin's secret comes from: PLUGIN:NAME=env:VAR or PLUGIN:NAME=file:PATH. \
                Repeatable; flags are tried before the config file's entries, and both before the \
                convention variable GWENNOL_SECRET_<PLUGIN>_<NAME>"
    )]
    pub secret: Vec<String>,

    /// The LLM_CHAT plugin to talk to. Default: the only one loaded.
    #[arg(long, value_name = "PLUGIN")]
    pub provider: Option<String>,

    /// The provider's model: sets `model` in its $config, by the
    /// convention that a provider's config names its model so. The
    /// bundled provider does; another provider's schema decides what
    /// the key means to it.
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,

    /// The system prompt.
    #[arg(long, value_name = "TEXT", conflicts_with = "system_file")]
    pub system: Option<String>,

    /// A file holding the system prompt.
    #[arg(long, value_name = "FILE")]
    pub system_file: Option<PathBuf>,

    /// The generation cap handed to the provider.
    #[arg(long, value_name = "N")]
    pub max_tokens: Option<u64>,

    /// Most provider rounds the turn may take.
    #[arg(long, value_name = "N")]
    pub max_rounds: Option<u32>,

    /// Ask the provider for buffered turns instead of streamed ones.
    #[arg(long)]
    pub no_stream: bool,

    /// Write the conversation as the provider saw it — system prompt,
    /// tools, messages and settings, the whole chat input — to FILE at
    /// the end, after a failure too.
    #[arg(long, value_name = "FILE")]
    pub transcript: Option<PathBuf>,

    /// Print mode: one turn, model text on stdout, the trace on stderr,
    /// no terminal needed. Implied when stdin or stdout is not a terminal.
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Write the host's log to FILE instead of stderr; -v sets its level.
    /// A session without this collects no log.
    #[arg(long, value_name = "FILE")]
    pub log: Option<PathBuf>,

    /// More detail on stderr: -v shows tool results whole and the
    /// host's info log, -vv its debug log.
    #[arg(short, long, action = ArgAction::Count)]
    pub verbose: u8,
}

impl Cli {
    /// The first turn: `prompt`, unless it is absent, `-`, or
    /// whitespace-only (a session then starts with an empty editor;
    /// print mode's own emptiness error is print mode's, not this).
    pub fn first_turn(&self) -> Option<String> {
        match self.prompt.as_deref() {
            Some("-") | None => None,
            Some(text) if text.trim().is_empty() => None,
            Some(text) => Some(text.to_string()),
        }
    }
}

/// A startup failure: reported once, exit status 2.
#[derive(Debug)]
pub struct Fatal(pub String);

impl<E: std::fmt::Display> From<E> for Fatal {
    fn from(e: E) -> Self {
        Self(e.to_string())
    }
}

/// The turn failed, or the session ended right after a failed turn.
pub const EXIT_TURN_FAILED: u8 = 1;
/// A usage, configuration or startup error.
pub const EXIT_USAGE: u8 = 2;
/// Cancelled by Ctrl-C in print mode, or forced by a second `/exit`.
pub const EXIT_CANCELLED: u8 = 130;

/// Which frontend a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// One task, model text on stdout, the trace on stderr, no input.
    Print,
    /// A terminal session: streamed output, a line editor, cancel.
    Interactive,
}

/// `Mode::Print` when `-p` was given, or stdin or stdout is not a
/// terminal; `Mode::Interactive` otherwise.
pub fn choose_mode(cli: &Cli, stdin_tty: bool, stdout_tty: bool) -> Mode {
    if cli.print || !stdin_tty || !stdout_tty {
        Mode::Print
    } else {
        Mode::Interactive
    }
}

/// Printed, once, before the log subscriber exists, when print mode was
/// chosen without `-p`.
pub const NO_TERMINAL_NOTICE: &str =
    "gwennol: no terminal on stdin or stdout; running in print mode, as -p";

/// `--allow`/`--deny` values in command-line order.
pub fn ordered_rule_flags(matches: &clap::ArgMatches) -> Vec<RuleSpec> {
    let mut flags: Vec<(usize, RuleSpec)> = Vec::new();
    for (name, decision) in [("allow", Decision::Allow), ("deny", Decision::Deny)] {
        let values = matches.get_many::<String>(name).into_iter().flatten();
        let indices = matches.indices_of(name).into_iter().flatten();
        for (text, index) in values.zip(indices) {
            flags.push((
                index,
                RuleSpec {
                    decision,
                    text: text.clone(),
                    plugin: None,
                    source: Source::Flag,
                },
            ));
        }
    }
    flags.sort_by_key(|(index, _)| *index);
    flags.into_iter().map(|(_, spec)| spec).collect()
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, FromArgMatches};

    use super::*;

    #[test]
    fn allow_and_deny_flags_keep_their_command_line_order() {
        let matches = Cli::command().get_matches_from([
            "gwennol",
            "--deny",
            "write:.git/**",
            "--allow",
            "read:**",
            "--allow",
            "write:**",
            "--deny",
            "spawn:*",
            "task",
        ]);
        let rules = ordered_rule_flags(&matches);
        let seen: Vec<(Decision, &str)> = rules
            .iter()
            .map(|r| (r.decision, r.text.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                (Decision::Deny, "write:.git/**"),
                (Decision::Allow, "read:**"),
                (Decision::Allow, "write:**"),
                (Decision::Deny, "spawn:*"),
            ]
        );
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// `-` and `None` are both read from stdin, at most once, and a
    /// whitespace-only prompt is empty: none of the three is a turn.
    /// Mutation: return `-` as a turn.
    #[test]
    fn the_prompt_argument_is_the_first_turn_or_nothing() {
        let cli = |prompt: Option<&str>| {
            let mut args = vec!["gwennol".to_string()];
            if let Some(p) = prompt {
                args.push(p.to_string());
            }
            let matches = Cli::command().get_matches_from(args);
            Cli::from_arg_matches(&matches).unwrap()
        };
        assert_eq!(cli(Some("hi")).first_turn(), Some("hi".to_string()));
        assert_eq!(cli(Some("-")).first_turn(), None);
        assert_eq!(cli(Some("  ")).first_turn(), None);
        assert_eq!(cli(None).first_turn(), None);
    }

    /// `-p`, a missing stdin terminal, or a missing stdout terminal:
    /// any one of the three is enough. Mutation: ignore `stdout_tty`.
    #[test]
    fn no_terminal_means_print_mode() {
        let matches = |print: bool| {
            let mut args = vec!["gwennol".to_string()];
            if print {
                args.push("-p".to_string());
            }
            let matches = Cli::command().get_matches_from(args);
            Cli::from_arg_matches(&matches).unwrap()
        };
        let print_cli = matches(true);
        let plain_cli = matches(false);
        for (print, stdin_tty, stdout_tty, expect) in [
            (true, true, true, Mode::Print),
            (true, true, false, Mode::Print),
            (true, false, true, Mode::Print),
            (true, false, false, Mode::Print),
            (false, true, true, Mode::Interactive),
            (false, true, false, Mode::Print),
            (false, false, true, Mode::Print),
            (false, false, false, Mode::Print),
        ] {
            let cli = if print { &print_cli } else { &plain_cli };
            assert_eq!(
                choose_mode(cli, stdin_tty, stdout_tty),
                expect,
                "print={print} stdin_tty={stdin_tty} stdout_tty={stdout_tty}"
            );
        }
    }
}
