//! What a run settles before its `Operator` drives a turn, and which
//! is the same for any `Operator`, except the default system prompt
//! below, which describes a headless run: an interactive frontend
//! needs its own, and `start` would have to grow a parameter for it,
//! since `system_prompt` is private and its sources are fixed — the
//! flags, the config, else a default built from the workspace. What is
//! shared: the workspace, the config and policy files, the compiled
//! policy, the secret sources, the plugins and their manifests, the
//! process environment, the kernel, and the session. A second
//! frontend added to this binary calls the same two functions and
//! supplies its own `Operator` in the closure, so it copies none of
//! this.

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
use crate::{Cli, Fatal, plugins};

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

/// Everything a frontend needs before its first turn: the config and
/// policy files, the compiled policy, the secret sources, the plugins
/// and their manifests, the process environment, the kernel, and the
/// session. `operator` is called at most once, after the plugins are loaded
/// and before the kernel boots, with the compiled policy, which it
/// takes; a borrow of the secret sources, which this module keeps
/// and the declared-secret warnings below consult; and the canonical
/// workspace. `workspace` must already be canonical, as [`workspace`]
/// returns it: the compiled policy is rooted at it verbatim. The
/// returned session has run no turn.
pub fn start(
    cli: &Cli,
    workspace: PathBuf,
    flag_rules: Vec<RuleSpec>,
    operator: impl FnOnce(Policy, &Secrets, &Path) -> Arc<dyn Operator>,
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
                tracing::warn!(
                    "plugin {plugin_name} declares secret {name:?} but no source has it: set {}",
                    secrets.describe_source(&plugin_name, &name)
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
    let system = system_prompt(cli, config.as_ref(), &workspace)?;
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
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let cli = Cli {
            task: None,
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
            verbose: 0,
        };
        let called = AtomicBool::new(false);
        let result = start(
            &cli,
            PathBuf::from(".").canonicalize().unwrap(),
            Vec::new(),
            |policy, secrets, workspace| {
                called.store(true, Ordering::SeqCst);
                Arc::new(Headless::new(
                    policy,
                    secrets.clone(),
                    workspace.to_path_buf(),
                    0,
                ))
            },
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
}
