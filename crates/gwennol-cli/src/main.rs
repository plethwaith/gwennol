//! Gwennol command-line frontend.
//!
//! The first `Operator` implementation: non-interactive by design, so
//! that policy is driven by flags and files rather than prompts. A run
//! is one task — one user turn — against the bundled plugins, with
//! every approval decided by a rule and traced to it on stderr. The TUI
//! comes later as a second `Operator`, not a rewrite.
//!
//! Exit status: 0 when the turn completed; 1 when it failed; 2 for a
//! usage, configuration or startup error; 130 when it was cancelled by
//! Ctrl-C.

#![forbid(unsafe_code)]

mod config;
mod operator;
mod plugins;
mod policy;
mod secrets;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{ArgAction, CommandFactory, FromArgMatches, Parser};
use gwennol_core::gwead::tokio_util::sync::CancellationToken;
use gwennol_core::{
    Decision, HostConfig, ProcessEnv, Session, SessionConfig, TurnError, host, resolve_provider,
};
use serde_json::Value;

use config::{Config, EnvMode, Loaded, PolicyFile};
use operator::Headless;
use policy::{Policy, RuleSpec, Source};
use secrets::Secrets;

/// Run one task headlessly, every approval decided by a rule.
#[derive(Debug, Parser)]
#[command(name = "gwennol", version, about, long_about = None)]
struct Cli {
    /// The task, as the user's one message. `-` or absent reads it
    /// from stdin.
    task: Option<String>,

    /// The workspace root: where relative paths resolve and commands
    /// run. Default: the current directory.
    #[arg(short = 'C', long, value_name = "DIR")]
    workspace: Option<PathBuf>,

    /// The config file. Default: $XDG_CONFIG_HOME/gwennol/config.toml,
    /// when it exists.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// A rules-only file, tried after the flags and before the config
    /// file's rules.
    #[arg(
        long,
        value_name = "FILE",
        help = "A file of [[rules]], tried after the flags and before the config file's rules"
    )]
    policy: Option<PathBuf>,

    /// Allow requests matching a rule (see `policy`). Repeatable; tried
    /// in the order given together with `deny`.
    #[arg(
        long,
        value_name = "RULE",
        action = ArgAction::Append,
        help = "Allow requests matching RULE (<kind>:<glob>; kinds read, write, list, spawn, http, any). \
                Repeatable; tried in the order given together with --deny. Nothing matching any rule is denied"
    )]
    allow: Vec<String>,

    /// Deny requests matching RULE. Repeatable; see --allow.
    #[arg(long, value_name = "RULE", action = ArgAction::Append)]
    deny: Vec<String>,

    /// The directory of bundled plugin manifests (`cargo xtask bundle`
    /// writes target/bundle/plugins).
    #[arg(long, value_name = "DIR", env = "GWENNOL_PLUGINS")]
    plugins: Option<PathBuf>,

    /// Trust PLUGIN to supply a script runtime. Repeatable; adds to
    /// the config file's list.
    #[arg(long = "trust-runtime", value_name = "PLUGIN")]
    trust_runtime: Vec<String>,

    /// Where a plugin's secret comes from (see `secrets`). Repeatable;
    /// flags are tried before the config file's entries.
    #[arg(
        long,
        value_name = "PLUGIN:NAME=SOURCE",
        help = "Where a plugin's secret comes from: PLUGIN:NAME=env:VAR or PLUGIN:NAME=file:PATH. \
                Repeatable; flags are tried before the config file's entries, and both before the \
                convention variable GWENNOL_SECRET_<PLUGIN>_<NAME>"
    )]
    secret: Vec<String>,

    /// The LLM_CHAT plugin to talk to. Default: the only one loaded.
    #[arg(long, value_name = "PLUGIN")]
    provider: Option<String>,

    /// The provider's model: sets `model` in its $config, by the
    /// convention that a provider's config names its model so. The
    /// bundled provider does; another provider's schema decides what
    /// the key means to it.
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// The system prompt.
    #[arg(long, value_name = "TEXT", conflicts_with = "system_file")]
    system: Option<String>,

    /// A file holding the system prompt.
    #[arg(long, value_name = "FILE")]
    system_file: Option<PathBuf>,

    /// The generation cap handed to the provider.
    #[arg(long, value_name = "N")]
    max_tokens: Option<u64>,

    /// Most provider rounds the turn may take.
    #[arg(long, value_name = "N")]
    max_rounds: Option<u32>,

    /// Ask the provider for buffered turns instead of streamed ones.
    #[arg(long)]
    no_stream: bool,

    /// Write the conversation as the provider saw it — system prompt,
    /// tools, messages and settings, the whole chat input — to FILE at
    /// the end, after a failure too.
    #[arg(long, value_name = "FILE")]
    transcript: Option<PathBuf>,

    /// More detail on stderr: -v shows tool results whole and the
    /// host's info log, -vv its debug log.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

/// A startup failure: reported once, exit status 2.
struct Fatal(String);

impl<E: std::fmt::Display> From<E> for Fatal {
    fn from(e: E) -> Self {
        Self(e.to_string())
    }
}

const EXIT_TURN_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_CANCELLED: u8 = 130;

fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    // --allow and --deny are one ordered list, which clap keeps only
    // as each flag's positions in the command line.
    let rules = ordered_rule_flags(&matches);

    // The host's log, on stderr beside the trace: -v raises the level;
    // RUST_LOG, when set, replaces it with its own directives.
    let level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .without_time()
        .with_target(cli.verbose >= 2)
        .init();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("gwennol: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match runtime.block_on(run(cli, rules)) {
        Ok(code) => code,
        Err(Fatal(message)) => {
            eprintln!("gwennol: {message}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// `--allow`/`--deny` values in command-line order.
fn ordered_rule_flags(matches: &clap::ArgMatches) -> Vec<RuleSpec> {
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

/// Everything after argument parsing.
async fn run(cli: Cli, flag_rules: Vec<RuleSpec>) -> Result<ExitCode, Fatal> {
    // ---- the workspace, canonical: the host shows canonical paths, so
    // rules must be rooted at the same spelling.
    let workspace = cli.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
    let workspace = workspace
        .canonicalize()
        .map_err(|e| Fatal(format!("workspace {}: {e}", workspace.display())))?;

    // ---- the task, before anything expensive: an empty one, or a
    // closed stdin, should not cost a kernel boot to find out.
    let task = match cli.task.as_deref() {
        Some("-") | None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| Fatal(format!("reading the task from stdin: {e}")))?;
            text
        }
        Some(text) => text.to_string(),
    };
    if task.trim().is_empty() {
        return Err(Fatal("the task is empty".into()));
    }

    // ---- the files.
    let config = load_config(cli.config.as_deref())?;
    let policy_file = match &cli.policy {
        Some(path) => Some(Loaded::<PolicyFile>::read(path)?),
        None => None,
    };

    // ---- the policy: flags, then the policy file, then the config.
    let mut specs = flag_rules;
    if let Some(file) = &policy_file {
        specs.extend(file.rules()?);
    }
    if let Some(file) = &config {
        specs.extend(file.rules()?);
    }
    let policy = Policy::compile(specs, &workspace)?;
    for rule in policy.rules() {
        tracing::info!(rule = %rule.spec(), "rule");
    }
    if policy.rules().is_empty() {
        tracing::warn!("no approval rules: every request will be denied");
    }

    // ---- the secrets: flags, then the config.
    let mut secret_rules = cli
        .secret
        .iter()
        .map(|s| secrets::Rule::parse_flag(s))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(file) = &config {
        secret_rules.extend(file.secrets()?);
    }
    let secrets = Secrets::new(secret_rules);

    // ---- the plugins.
    let config_dir = config
        .as_ref()
        .and_then(|c| c.value.plugins.dir.as_deref().map(|d| c.resolve(d)));
    let (plugins_dir, origin) = plugins::resolve_dir(cli.plugins.clone(), config_dir)?;
    tracing::info!(dir = %plugins_dir.display(), %origin, "plugins");
    let manifests = plugins::load(&plugins_dir)?;
    if manifests.is_empty() {
        return Err(Fatal(format!(
            "no plugin manifests under {}",
            plugins_dir.display()
        )));
    }
    let mut trusted = cli.trust_runtime.clone();
    if let Some(file) = &config {
        trusted.extend(file.value.plugins.trust_runtimes.iter().cloned());
    }

    // ---- the process environment.
    let process_env = match &config {
        Some(file) => match file.value.process.env {
            EnvMode::Inherit => ProcessEnv::Inherit,
            EnvMode::Allowlist => {
                let mut names: Vec<String> = host::DEFAULT_ENV_ALLOWLIST
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                names.extend(file.value.process.allow.iter().cloned());
                ProcessEnv::AllowList(names)
            }
        },
        None => ProcessEnv::default(),
    };

    // ---- boot, and register.
    let operator = Arc::new(Headless::new(
        policy,
        secrets,
        workspace.clone(),
        cli.verbose,
    ));
    let mut kernel = gwennol_core::boot_with(HostConfig {
        operator: operator.clone(),
        workspace_root: workspace.clone(),
        process_env,
        trusted_step_type_providers: trusted,
        action_timeout: gwennol_core::DEFAULT_ACTION_TIMEOUT,
    })?;
    for plugin in &manifests {
        kernel
            .register_plugin_from_json(&plugin.manifest.to_string())
            .map_err(|e| Fatal(format!("{}: {e}", plugin.path.display())))?;
        tracing::info!(plugin = %plugin.name(), file = %plugin.path.display(), "registered");
        for name in plugin.uses_secrets() {
            let plugin_name = plugin.name();
            if !operator.secrets().is_available(&plugin_name, &name) {
                tracing::warn!(
                    "plugin {plugin_name} declares secret {name:?} but no source has it: set {}",
                    operator.secrets().describe_source(&plugin_name, &name)
                );
            }
        }
    }
    let kernel = kernel.into_arc();

    // ---- the session.
    let provider = cli.provider.clone().or_else(|| {
        config
            .as_ref()
            .and_then(|c| c.value.session.provider.clone())
    });
    let mut plugin_configs: BTreeMap<String, Value> = BTreeMap::new();
    if let Some(file) = &config {
        for (name, table) in &file.value.plugin_config {
            let value = serde_json::to_value(table)
                .map_err(|e| Fatal(format!("[plugin_config.{name}]: {e}")))?;
            plugin_configs.insert(name.clone(), value);
        }
    }
    if let Some(model) = &cli.model {
        // --model needs to know which plugin's config to set: the
        // session's provider, resolved by the loop's own rule.
        let name = resolve_provider(&kernel, provider.as_deref())
            .map_err(|e| Fatal(format!("--model: {e}")))?;
        plugin_configs
            .entry(name)
            .or_insert_with(|| Value::Object(Default::default()))["model"] =
            Value::String(model.clone());
    }
    let system = system_prompt(&cli, config.as_ref(), &workspace)?;
    let session_file = config.as_ref().map(|c| &c.value.session);
    let mut session_config = SessionConfig {
        provider,
        system: Some(system),
        max_tokens: cli.max_tokens.or(session_file.and_then(|s| s.max_tokens)),
        stream: !cli.no_stream && session_file.and_then(|s| s.stream).unwrap_or(true),
        plugin_configs,
        ..SessionConfig::default()
    };
    if let Some(rounds) = cli.max_rounds.or(session_file.and_then(|s| s.max_rounds)) {
        session_config.max_rounds = rounds;
    }
    let mut session = Session::new(kernel, session_config)?;
    tracing::info!(provider = session.provider(), "session");

    // ---- one turn, cancellable.
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("gwennol: interrupted; cancelling the turn");
                cancel.cancel();
            }
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("gwennol: interrupted again; exiting");
                std::process::exit(i32::from(EXIT_CANCELLED));
            }
        });
    }
    let outcome = session.turn(&task, &cancel).await;
    let code = match outcome {
        Ok(outcome) => {
            eprintln!(
                "gwennol: done ({:?}): {} round{}, {} tokens in, {} out",
                outcome.stop_reason,
                outcome.rounds,
                if outcome.rounds == 1 { "" } else { "s" },
                outcome.usage.input_tokens,
                outcome.usage.output_tokens
            );
            ExitCode::SUCCESS
        }
        Err(TurnError::Cancelled) => {
            eprintln!("gwennol: cancelled");
            ExitCode::from(EXIT_CANCELLED)
        }
        Err(e) => {
            eprintln!("gwennol: turn failed: {e}");
            ExitCode::from(EXIT_TURN_FAILED)
        }
    };
    // After the outcome is reported, so a transcript that cannot be
    // written never hides how the turn went. It is still a failure of
    // what was asked for — but the turn's own failure or cancellation
    // is the more important fact, and a wrapper keying on that status
    // must keep seeing it.
    if let Some(path) = &cli.transcript
        && let Err(Fatal(message)) = write_transcript(path, &session.chat_input())
    {
        eprintln!("gwennol: {message}");
        if code == ExitCode::SUCCESS {
            return Ok(ExitCode::from(EXIT_USAGE));
        }
    }
    Ok(code)
}

/// The config file: the one named, which must exist, else the default
/// location when it does.
fn load_config(flag: Option<&Path>) -> Result<Option<Loaded<Config>>, Fatal> {
    let path = match flag {
        Some(path) => path.to_path_buf(),
        None => match config::default_path() {
            Some(path) if path.is_file() => path,
            _ => return Ok(None),
        },
    };
    let loaded = Loaded::<Config>::read(&path)?;
    tracing::info!(file = %path.display(), "config");
    Ok(Some(loaded))
}

/// The system prompt: the flag, the flag's file, the config's text,
/// the config's file, else the default.
fn system_prompt(
    cli: &Cli,
    config: Option<&Loaded<Config>>,
    workspace: &Path,
) -> Result<String, Fatal> {
    if let Some(text) = &cli.system {
        return Ok(text.clone());
    }
    if let Some(path) = &cli.system_file {
        return read_prompt(path);
    }
    if let Some(file) = config {
        if let Some(text) = &file.value.session.system {
            return Ok(text.clone());
        }
        if let Some(path) = &file.value.session.system_file {
            return read_prompt(&file.resolve(path));
        }
    }
    Ok(default_system_prompt(workspace))
}

fn read_prompt(path: &Path) -> Result<String, Fatal> {
    std::fs::read_to_string(path)
        .map_err(|e| Fatal(format!("system prompt {}: {e}", path.display())))
}

/// What the model is told when nothing else is configured.
fn default_system_prompt(workspace: &Path) -> String {
    format!(
        "You are Gwennol, a coding agent working headlessly in the directory {}. \
         Relative paths resolve against that directory and commands run in it. \
         Use the tools to read, search and change files and to run commands; \
         some requests may be refused by policy, in which case work around them \
         or say what you could not do. Act on the task directly, then report \
         what you did.",
        workspace.display()
    )
}

/// The whole chat input, pretty-printed: what the provider was handed
/// on the last round plus its answer, so the file is a request someone
/// can read or replay, not just the messages.
fn write_transcript(path: &Path, chat_input: &Value) -> Result<(), Fatal> {
    let text = serde_json::to_string_pretty(chat_input).expect("a Value serialises");
    std::fs::write(path, text).map_err(|e| Fatal(format!("transcript {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
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
}
