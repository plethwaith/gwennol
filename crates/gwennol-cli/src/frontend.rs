//! What a run settles before its `Operator` drives a turn: the
//! workspace, the config and policy files, the compiled policy, the
//! secret sources, the plugins and their manifests, the process
//! environment, the kernel, and the session. `start` takes the run's
//! `Mode`, which picks how the default system prompt describes the
//! run. Both frontends call the same two functions and supply their
//! own `Operator` in the closure, so neither copies any of this.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gwennol_core::{
    HostConfig, Operator, ProcessEnv, Session, SessionConfig, host, resolve_provider,
};
use serde_json::Value;

use crate::config::{Config, EnvMode, Loaded, PolicyFile};
use crate::policy::{Policy, RuleSpec};
use crate::secrets::{self, Secrets};
use crate::{Cli, Fatal, Mode, plugins};

/// How the default system prompt describes a print run.
pub const PRINT_RUN: &str = "This is a print run: one task, and no one to ask. Every request to read, change or run something is allowed or denied by rules set before the run, and a request no rule matches is denied.";

/// How the default system prompt describes a session.
pub const SESSION: &str = "This is an interactive session: a person reads your output as it arrives and sends each turn. Every request to read, change or run something is decided by a rule, or put to that person when no rule decides it.";

/// What the default system prompt says about files and commands, in
/// either mode.
pub const FILE_TOOLS: &str = "Use read, grep, write and edit for files: read takes a range of lines, and edit changes one exact string in place. Keep bash for running commands. A read, write or edit is decided by the file's path; grep and bash are decided by their command line.";

/// The workspace, canonical: the host shows canonical paths, so rules
/// must be rooted at the same spelling. Settled before the frontend
/// reads anything else, so a workspace that does not exist is the
/// first error a run reports.
pub fn workspace(cli: &Cli) -> Result<PathBuf, Fatal> {
    let workspace = cli.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
    let workspace = workspace
        .canonicalize()
        .map_err(|e| Fatal(format!("workspace {}: {e}", workspace.display())))?;
    Ok(workspace)
}

/// Everything a frontend needs before its first turn: the config
/// and policy files, the compiled policy, the secret sources,
/// the plugins and their manifests, the process environment,
/// the kernel, and the session. `operator` is called at most once,
/// after the plugins are loaded and before the kernel boots, with the
/// compiled policy, which it takes; a borrow of the secret sources,
/// which this module keeps and the declared-secret warnings below
/// consult; and the canonical workspace. `workspace` must already
/// be canonical, as [`workspace`] returns it: the compiled policy
/// is rooted at it verbatim. `warnings` receives the startup warnings
/// a frontend with no stderr should still show — one per declared
/// secret with no source — in the same words their log lines carry.
/// The no-rules warning goes only to the log: it describes print
/// mode's default, and a session asks instead. Other warnings raised
/// while starting (`policy::walk`'s unresolvable-prefix one) go only
/// to the log too. The returned session has run no turn.
/// `mode` picks the default system prompt's description of the run.
pub fn start(
    cli: &Cli,
    workspace: PathBuf,
    flag_rules: Vec<RuleSpec>,
    mode: Mode,
    operator: impl FnOnce(Policy, &Secrets, &Path) -> Arc<dyn Operator>,
    warnings: &mut Vec<String>,
) -> Result<Session, Fatal> {
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
    let operator = operator(policy, &secrets, &workspace);
    let mut kernel = gwennol_core::boot_with(HostConfig {
        operator,
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
            if !secrets.is_available(&plugin_name, &name) {
                let message = format!(
                    "plugin {plugin_name} declares secret {name:?} but no source has it: set {}",
                    secrets.describe_source(&plugin_name, &name)
                );
                tracing::warn!("{message}");
                warnings.push(message);
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
    let system = system_prompt(cli, config.as_ref(), &workspace, mode)?;
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
    let session = Session::new(kernel, session_config)?;
    tracing::info!(provider = session.provider(), "session");
    Ok(session)
}

/// The config file: the one named, which must exist, else the default
/// location when it does.
fn load_config(flag: Option<&Path>) -> Result<Option<Loaded<Config>>, Fatal> {
    let path = match flag {
        Some(path) => path.to_path_buf(),
        None => match crate::config::default_path() {
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
    mode: Mode,
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
    Ok(default_system_prompt(workspace, mode))
}

fn read_prompt(path: &Path) -> Result<String, Fatal> {
    std::fs::read_to_string(path)
        .map_err(|e| Fatal(format!("system prompt {}: {e}", path.display())))
}

/// What the model is told when nothing else is configured: where it
/// is, how the run is driven, and which tools to use for files.
/// Written for the bundled tools.
fn default_system_prompt(workspace: &Path, mode: Mode) -> String {
    let run = match mode {
        Mode::Print => PRINT_RUN,
        Mode::Interactive => SESSION,
    };
    format!(
        "You are Gwennol, a coding agent working in the directory {}. \
         Relative paths resolve against that directory and commands run in it. \
         {run} {FILE_TOOLS} When a request is refused, work around it or say \
         what you could not do. Act on the task directly, then report what \
         you did.",
        workspace.display()
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::operator::Headless;

    /// `operator` is called after the plugins have loaded, not before:
    /// a startup failure ahead of that point — here, `--plugins`
    /// naming a directory that does not exist — never calls it.
    #[test]
    fn the_operator_factory_is_not_called_before_the_plugins_load() {
        // An empty file, not `config: None`: with `None`, `load_config`
        // falls through to `default_path()`, so a real
        // `$XDG_CONFIG_HOME/gwennol/config.toml` on the machine running
        // this test would be read instead, and whatever it contains
        // could fail before the plugins ever load.
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let cli = Cli {
            prompt: None,
            workspace: None,
            config: Some(config_path),
            policy: None,
            allow: Vec::new(),
            deny: Vec::new(),
            plugins: Some(PathBuf::from("/nonexistent/plugins-dir")),
            trust_runtime: Vec::new(),
            secret: Vec::new(),
            provider: None,
            model: None,
            system: None,
            system_file: None,
            max_tokens: None,
            max_rounds: None,
            no_stream: false,
            transcript: None,
            print: false,
            log: None,
            verbose: 0,
        };
        let called = AtomicBool::new(false);
        let result = start(
            &cli,
            PathBuf::from(".").canonicalize().unwrap(),
            Vec::new(),
            Mode::Print,
            |policy, secrets, workspace| {
                called.store(true, Ordering::SeqCst);
                Arc::new(Headless::new(
                    policy,
                    secrets.clone(),
                    workspace.to_path_buf(),
                    0,
                ))
            },
            &mut Vec::new(),
        );
        let err = result.expect_err("expected a startup failure");
        assert!(
            err.0.contains("/nonexistent/plugins-dir"),
            "the run failed before the plugins were loaded: {}",
            err.0
        );
        assert!(
            !called.load(Ordering::SeqCst),
            "the operator factory ran before the plugins failed to load"
        );
    }

    /// The default prompt names the run it describes and not the
    /// other, and neither calls the run "headless". Mutation:
    /// `default_system_prompt` ignoring `mode` makes both prompts
    /// equal, so the `assert_ne!` below fails.
    #[test]
    fn the_default_prompt_says_how_the_run_is_driven() {
        let workspace = PathBuf::from("/tmp/ws");
        let print = default_system_prompt(&workspace, Mode::Print);
        assert!(print.contains(PRINT_RUN), "{print}");
        assert!(print.contains(FILE_TOOLS), "{print}");
        assert!(print.contains("/tmp/ws"), "{print}");
        assert!(!print.contains(SESSION), "{print}");
        assert!(!print.to_lowercase().contains("headless"), "{print}");

        let session = default_system_prompt(&workspace, Mode::Interactive);
        assert!(session.contains(SESSION), "{session}");
        assert!(session.contains(FILE_TOOLS), "{session}");
        assert!(!session.contains(PRINT_RUN), "{session}");
        assert!(!session.to_lowercase().contains("headless"), "{session}");
        assert_ne!(print, session);
    }

    /// `--system` wins over the default in either mode.
    /// Mutation: check the default before the flag.
    #[test]
    fn explicit_system_text_wins_over_the_default_in_either_mode() {
        let cli = Cli {
            prompt: None,
            workspace: None,
            config: None,
            policy: None,
            allow: Vec::new(),
            deny: Vec::new(),
            plugins: None,
            trust_runtime: Vec::new(),
            secret: Vec::new(),
            provider: None,
            model: None,
            system: Some("mine".to_string()),
            system_file: None,
            max_tokens: None,
            max_rounds: None,
            no_stream: false,
            transcript: None,
            print: false,
            log: None,
            verbose: 0,
        };
        let text = system_prompt(&cli, None, &PathBuf::from("/tmp/ws"), Mode::Interactive)
            .expect("an explicit --system never touches the filesystem");
        assert_eq!(text, "mine");

        let text = system_prompt(&cli, None, &PathBuf::from("/tmp/ws"), Mode::Print)
            .expect("an explicit --system never touches the filesystem");
        assert_eq!(text, "mine");
    }
}
