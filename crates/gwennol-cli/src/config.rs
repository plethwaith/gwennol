//! The config file and the policy file.
//!
//! Both are TOML. The config file (`--config`, else
//! `$XDG_CONFIG_HOME/gwennol/config.toml` when it exists) holds
//! everything a run needs that is not the task: where the plugins are,
//! which may supply script runtimes, the session's provider and
//! prompt, each plugin's `$config`, where secrets come from, what
//! environment a spawned process gets, and approval rules. The policy
//! file (`--policy`) holds rules only, so a set of rules can travel
//! between projects on its own.
//!
//! Flags override the config file field by field; rules are not
//! overridden but *ordered* — flags, then the policy file, then the
//! config file (see [`crate::policy`]). Relative paths in a file
//! resolve against the directory the file is in.
//!
//! The default location is deliberately outside the workspace. A
//! policy the agent can rewrite from inside the workspace it is editing
//! would govern the next run; a file under the user's config directory
//! is reachable only by a rule that names it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gwennol_core::Decision;
use serde::Deserialize;

use crate::policy::{RuleSpec, Source};
use crate::secrets;

/// Where the plugins are and which may supply runtimes.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plugins {
    /// The directory of registrable manifests, as `cargo xtask bundle`
    /// writes under `target/bundle/plugins`.
    pub dir: Option<PathBuf>,
    /// Plugin names trusted to supply a script runtime — the embedder
    /// half of Gwead's two-key authorisation (`docs/SUBSTRATE.md`).
    #[serde(default)]
    pub trust_runtimes: Vec<String>,
}

/// The session.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Session {
    /// The `LLM_CHAT` plugin to talk to; resolved by role when unset.
    pub provider: Option<String>,
    /// The system prompt.
    pub system: Option<String>,
    /// A file holding the system prompt; `system` wins when both are set.
    pub system_file: Option<PathBuf>,
    /// The generation cap handed to the provider.
    pub max_tokens: Option<u64>,
    /// Most provider rounds one turn may take.
    pub max_rounds: Option<u32>,
    /// Streamed (the default) or buffered turns.
    pub stream: Option<bool>,
}

/// One `[[secrets]]` entry: exactly one of `env` or `file`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    /// The plugin, as its manifest names it.
    pub plugin: String,
    /// The secret's name in `usesSecrets`.
    pub name: String,
    /// Read from this environment variable.
    pub env: Option<String>,
    /// Read from this file.
    pub file: Option<PathBuf>,
}

/// What a spawned process's environment is built from.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EnvMode {
    /// The host's default allow-list, plus [`Process::allow`].
    #[default]
    Allowlist,
    /// The whole environment this process was launched with. An
    /// explicit choice: see `gwennol_core::ProcessEnv`.
    Inherit,
}

/// The process environment policy.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Process {
    /// Allow-list or inherit.
    #[serde(default)]
    pub env: EnvMode,
    /// Variables passed through on top of the default allow-list.
    #[serde(default)]
    pub allow: Vec<String>,
}

/// One `[[rules]]` entry: exactly one of `allow` or `deny`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Allow requests matching this rule text.
    pub allow: Option<String>,
    /// Deny requests matching this rule text.
    pub deny: Option<String>,
    /// Only requests from this plugin.
    pub plugin: Option<String>,
}

/// The config file.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `[plugins]`.
    #[serde(default)]
    pub plugins: Plugins,
    /// `[session]`.
    #[serde(default)]
    pub session: Session,
    /// `[plugin_config.<plugin>]`: that plugin's `$config`, verbatim.
    #[serde(default)]
    pub plugin_config: BTreeMap<String, toml::Table>,
    /// `[[secrets]]`.
    #[serde(default)]
    pub secrets: Vec<Secret>,
    /// `[process]`.
    #[serde(default)]
    pub process: Process,
    /// `[[rules]]`.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// The policy file: rules and nothing else.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    /// `[[rules]]`.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// Why a file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Not readable.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// Not the TOML this frontend reads.
    #[error("{path}: {source}")]
    Toml {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: toml::de::Error,
    },
    /// A `[[rules]]` entry has neither or both of `allow` and `deny`.
    #[error("{path}: rule {index}: exactly one of `allow` or `deny` must be set")]
    RuleShape {
        /// The file.
        path: PathBuf,
        /// One-based position.
        index: usize,
    },
    /// A `[[secrets]]` entry has neither or both of `env` and `file`.
    #[error("{path}: secrets entry {index}: exactly one of `env` or `file` must be set")]
    SecretShape {
        /// The file.
        path: PathBuf,
        /// One-based position.
        index: usize,
    },
    /// `[process] env = "inherit"` with an `allow` list, which would be
    /// silently ignored: inheriting passes everything.
    #[error("{path}: [process] allow has no effect under env = \"inherit\"; remove one")]
    InheritWithAllow {
        /// The file.
        path: PathBuf,
    },
}

/// A file that was read, remembering where from.
#[derive(Debug, Clone)]
pub struct Loaded<T> {
    /// The file.
    pub path: PathBuf,
    /// Its content.
    pub value: T,
}

impl<T> Loaded<T> {
    /// Resolve a path written in this file: relative to its directory.
    pub fn resolve(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            return path.to_path_buf();
        }
        self.path
            .parent()
            .map(|dir| dir.join(path))
            .unwrap_or_else(|| path.to_path_buf())
    }
}

fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Loaded<T>, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value = toml::from_str(&text).map_err(|source| ConfigError::Toml {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Loaded {
        path: path.to_path_buf(),
        value,
    })
}

/// Convert `[[rules]]` entries to specs naming their file and position.
fn rule_specs(path: &Path, rules: &[Rule]) -> Result<Vec<RuleSpec>, ConfigError> {
    rules
        .iter()
        .enumerate()
        .map(|(i, rule)| {
            let index = i + 1;
            let (decision, text) = match (&rule.allow, &rule.deny) {
                (Some(text), None) => (Decision::Allow, text.clone()),
                (None, Some(text)) => (Decision::Deny, text.clone()),
                _ => {
                    return Err(ConfigError::RuleShape {
                        path: path.to_path_buf(),
                        index,
                    });
                }
            };
            Ok(RuleSpec {
                decision,
                text,
                plugin: rule.plugin.clone(),
                source: Source::File {
                    path: path.to_path_buf(),
                    index,
                },
            })
        })
        .collect()
}

impl Loaded<Config> {
    /// Read a config file.
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
        let loaded: Self = read(path)?;
        // Validate the shapes serde cannot express, so a bad entry is
        // refused at startup rather than when it is first needed.
        loaded.rules()?;
        loaded.secrets()?;
        if loaded.value.process.env == EnvMode::Inherit && !loaded.value.process.allow.is_empty() {
            return Err(ConfigError::InheritWithAllow { path: loaded.path });
        }
        Ok(loaded)
    }

    /// The file's approval rules, in order.
    pub fn rules(&self) -> Result<Vec<RuleSpec>, ConfigError> {
        rule_specs(&self.path, &self.value.rules)
    }

    /// The file's secret sources, in order, file paths resolved.
    pub fn secrets(&self) -> Result<Vec<secrets::Rule>, ConfigError> {
        self.value
            .secrets
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let source = match (&entry.env, &entry.file) {
                    (Some(var), None) => secrets::Source::Env(var.clone()),
                    (None, Some(file)) => secrets::Source::File(self.resolve(file)),
                    _ => {
                        return Err(ConfigError::SecretShape {
                            path: self.path.clone(),
                            index: i + 1,
                        });
                    }
                };
                Ok(secrets::Rule {
                    plugin: entry.plugin.clone(),
                    name: entry.name.clone(),
                    source,
                    origin: format!("{} secrets entry {}", self.path.display(), i + 1),
                })
            })
            .collect()
    }
}

impl Loaded<PolicyFile> {
    /// Read a policy file.
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
        let loaded: Self = read(path)?;
        loaded.rules()?;
        Ok(loaded)
    }

    /// The file's approval rules, in order.
    pub fn rules(&self) -> Result<Vec<RuleSpec>, ConfigError> {
        rule_specs(&self.path, &self.value.rules)
    }
}

/// The config file's default location: `$XDG_CONFIG_HOME/gwennol/
/// config.toml`, with `$HOME/.config` standing in for an unset
/// `$XDG_CONFIG_HOME`. `None` when neither variable is set.
pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("gwennol").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn a_full_config_reads_and_paths_resolve_beside_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "config.toml",
            r#"
[plugins]
dir = "plugins"
trust_runtimes = ["provider-anthropic"]

[session]
provider = "provider-anthropic"
system_file = "system.md"
max_tokens = 4096
max_rounds = 16
stream = false

[plugin_config.provider-anthropic]
model = "claude-fixture"
extra = { temperature = 0.2 }

[[secrets]]
plugin = "provider-anthropic"
name = "api_key"
file = "anthropic.key"

[process]
allow = ["CARGO_HOME"]

[[rules]]
allow = "http:https://api.anthropic.com/*"
plugin = "provider-anthropic"

[[rules]]
deny = "write:.git/**"
"#,
        );
        let loaded = Loaded::<Config>::read(&path).unwrap();
        let c = &loaded.value;
        assert_eq!(
            loaded.resolve(Path::new("plugins")),
            dir.path().join("plugins")
        );
        assert_eq!(c.plugins.trust_runtimes, ["provider-anthropic"]);
        assert_eq!(c.session.stream, Some(false));
        assert_eq!(c.session.max_rounds, Some(16));
        let provider = &c.plugin_config["provider-anthropic"];
        assert_eq!(provider["model"].as_str(), Some("claude-fixture"));
        assert_eq!(c.process.env, EnvMode::Allowlist);
        assert_eq!(c.process.allow, ["CARGO_HOME"]);

        let secrets = loaded.secrets().unwrap();
        assert_eq!(
            secrets[0].source,
            secrets::Source::File(dir.path().join("anthropic.key"))
        );
        assert!(secrets[0].origin.ends_with("secrets entry 1"));

        let rules = loaded.rules().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].decision, Decision::Allow);
        assert_eq!(rules[0].plugin.as_deref(), Some("provider-anthropic"));
        assert_eq!(rules[1].decision, Decision::Deny);
        assert_eq!(
            rules[1].source,
            Source::File {
                path: path.clone(),
                index: 2
            }
        );
    }

    #[test]
    fn an_empty_file_is_a_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "config.toml", "");
        let loaded = Loaded::<Config>::read(&path).unwrap();
        assert_eq!(loaded.value, Config::default());
    }

    #[test]
    fn unknown_keys_and_malformed_entries_are_refused_at_read() {
        let dir = tempfile::tempdir().unwrap();
        let typo = write(dir.path(), "typo.toml", "[sesion]\nprovider = 'x'\n");
        let err = Loaded::<Config>::read(&typo).unwrap_err().to_string();
        assert!(err.contains("typo.toml"), "{err}");
        assert!(err.contains("sesion"), "{err}");

        let both = write(
            dir.path(),
            "both.toml",
            "[[rules]]\nallow = 'any'\ndeny = 'any'\n",
        );
        let err = Loaded::<Config>::read(&both).unwrap_err().to_string();
        assert!(err.contains("rule 1: exactly one of"), "{err}");

        let neither = write(
            dir.path(),
            "neither.toml",
            "[[secrets]]\nplugin = 'p'\nname = 'k'\n",
        );
        let err = Loaded::<Config>::read(&neither).unwrap_err().to_string();
        assert!(err.contains("secrets entry 1: exactly one of"), "{err}");

        // Inheriting the environment passes everything, so an allow
        // list beside it is a contradiction, not a no-op.
        let inherit = write(
            dir.path(),
            "inherit.toml",
            "[process]\nenv = 'inherit'\nallow = ['X']\n",
        );
        let err = Loaded::<Config>::read(&inherit).unwrap_err().to_string();
        assert!(err.contains("allow has no effect"), "{err}");
        let inherit = write(
            dir.path(),
            "inherit-ok.toml",
            "[process]\nenv = 'inherit'\n",
        );
        assert_eq!(
            Loaded::<Config>::read(&inherit).unwrap().value.process.env,
            EnvMode::Inherit
        );

        // A policy file holds rules and nothing else.
        let policy = write(dir.path(), "policy.toml", "[session]\nprovider = 'x'\n");
        assert!(Loaded::<PolicyFile>::read(&policy).is_err());
        let policy = write(dir.path(), "ok.toml", "[[rules]]\nallow = 'read:**'\n");
        let rules = Loaded::<PolicyFile>::read(&policy)
            .unwrap()
            .rules()
            .unwrap();
        assert_eq!(rules[0].text, "read:**");
    }

    #[test]
    fn a_missing_file_names_itself() {
        let err = Loaded::<Config>::read(Path::new("/nonexistent/gwennol.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("/nonexistent/gwennol.toml: "), "{err}");
    }
}
