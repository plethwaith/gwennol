//! Where a plugin's secrets come from.
//!
//! The host asks the operator for `(plugin, name)` pairs a manifest
//! declared in `usesSecrets`, and only those. This frontend answers from
//! explicit sources — `--secret` flags and `[[secrets]]` entries, tried
//! in that order — and, when none names the pair, from the environment
//! variable the naming convention gives it:
//! `GWENNOL_SECRET_<PLUGIN>_<NAME>`, upper-cased, with every character
//! outside `[A-Za-z0-9]` written as `_`. So the bundled provider's key
//! is `GWENNOL_SECRET_PROVIDER_ANTHROPIC_API_KEY` unless a source says
//! otherwise.
//!
//! Values are read when asked for, not at startup, and never logged;
//! which source answered is logged at debug level.

use std::path::{Path, PathBuf};

/// One place a secret value can be read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An environment variable of this process.
    Env(String),
    /// A file, whose content — less one trailing newline — is the value.
    File(PathBuf),
}

/// A `(plugin, name)` pair and where to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The plugin, as named in its manifest.
    pub plugin: String,
    /// The secret's name in that manifest's `usesSecrets`.
    pub name: String,
    /// Where the value is.
    pub source: Source,
    /// Where the rule was written, for the log: `--secret` or a file.
    pub origin: String,
}

/// Why a `--secret` flag could not be read.
#[derive(Debug, thiserror::Error)]
#[error("--secret {0:?}: expected PLUGIN:NAME=env:VAR or PLUGIN:NAME=file:PATH")]
pub struct ParseError(pub String);

impl Rule {
    /// Parse a `--secret` value: `PLUGIN:NAME=env:VAR` or
    /// `PLUGIN:NAME=file:PATH`. A relative file path is taken as given,
    /// relative to the working directory the command runs in.
    pub fn parse_flag(text: &str) -> Result<Self, ParseError> {
        let err = || ParseError(text.to_string());
        let (pair, source) = text.split_once('=').ok_or_else(err)?;
        let (plugin, name) = pair.split_once(':').ok_or_else(err)?;
        if plugin.is_empty() || name.is_empty() {
            return Err(err());
        }
        let source = match source.split_once(':') {
            Some(("env", var)) if !var.is_empty() => Source::Env(var.to_string()),
            Some(("file", path)) if !path.is_empty() => Source::File(PathBuf::from(path)),
            _ => return Err(err()),
        };
        Ok(Self {
            plugin: plugin.to_string(),
            name: name.to_string(),
            source,
            origin: "--secret".to_string(),
        })
    }
}

/// The environment variable the naming convention assigns a pair.
pub fn convention_var(plugin: &str, name: &str) -> String {
    let mangle = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("GWENNOL_SECRET_{}_{}", mangle(plugin), mangle(name))
}

/// Every source, in the order they are tried.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    rules: Vec<Rule>,
}

/// How a lookup went, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// A rule answered.
    Rule {
        /// Which one.
        origin: String,
        /// From where.
        source: Source,
    },
    /// The convention variable answered.
    Convention(String),
}

impl Secrets {
    /// Sources in priority order: flags before files, each in the
    /// order given.
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    /// The first source for `(plugin, name)`: a rule naming it, else
    /// the convention variable. Says where, not what.
    fn source_for(&self, plugin: &str, name: &str) -> (Source, Found) {
        match self
            .rules
            .iter()
            .find(|r| r.plugin == plugin && r.name == name)
        {
            Some(rule) => (
                rule.source.clone(),
                Found::Rule {
                    origin: rule.origin.clone(),
                    source: rule.source.clone(),
                },
            ),
            None => {
                let var = convention_var(plugin, name);
                (Source::Env(var.clone()), Found::Convention(var))
            }
        }
    }

    /// Read the value for `(plugin, name)`, and say where it came from.
    /// `None` when the source has nothing: an unset variable, an
    /// unreadable file.
    pub fn lookup(&self, plugin: &str, name: &str) -> Option<(String, Found)> {
        let (source, found) = self.source_for(plugin, name);
        read(&source).map(|value| (value, found))
    }

    /// Whether the source for `(plugin, name)` would answer, without
    /// keeping the value: the startup check that warns before a plugin
    /// runs and finds its secret missing.
    pub fn is_available(&self, plugin: &str, name: &str) -> bool {
        let (source, _) = self.source_for(plugin, name);
        read(&source).is_some()
    }

    /// What a missing pair should be set as, for the warning.
    pub fn describe_source(&self, plugin: &str, name: &str) -> String {
        match self.source_for(plugin, name) {
            (Source::Env(var), Found::Convention(_)) => {
                format!("environment variable {var} (or a [[secrets]] entry / --secret)")
            }
            (Source::Env(var), _) => format!("environment variable {var}"),
            (Source::File(path), _) => format!("file {}", path.display()),
        }
    }
}

fn read(source: &Source) -> Option<String> {
    match source {
        Source::Env(var) => std::env::var(var).ok(),
        Source::File(path) => read_file(path),
    }
}

fn read_file(path: &Path) -> Option<String> {
    let mut text = std::fs::read_to_string(path).ok()?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_convention_mangles_names_predictably() {
        assert_eq!(
            convention_var("provider-anthropic", "api_key"),
            "GWENNOL_SECRET_PROVIDER_ANTHROPIC_API_KEY"
        );
        assert_eq!(convention_var("a.b", "c d"), "GWENNOL_SECRET_A_B_C_D");
    }

    #[test]
    fn flags_parse_both_forms_and_refuse_the_rest() {
        let env = Rule::parse_flag("provider-anthropic:api_key=env:ANTHROPIC_API_KEY").unwrap();
        assert_eq!(env.plugin, "provider-anthropic");
        assert_eq!(env.name, "api_key");
        assert_eq!(env.source, Source::Env("ANTHROPIC_API_KEY".into()));
        let file = Rule::parse_flag("p:n=file:/run/secrets/key").unwrap();
        assert_eq!(file.source, Source::File(PathBuf::from("/run/secrets/key")));
        for bad in [
            "p:n",
            "p:n=env:",
            "p:n=vault:x",
            ":n=env:V",
            "p:=env:V",
            "pn=env:V",
        ] {
            assert!(Rule::parse_flag(bad).is_err(), "{bad} parsed");
        }
    }

    #[test]
    fn rules_come_before_the_convention_and_files_lose_one_newline() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        std::fs::write(&key, "sk-from-file\n").unwrap();
        let secrets = Secrets::new(vec![Rule {
            plugin: "p".into(),
            name: "k".into(),
            source: Source::File(key.clone()),
            origin: "test".into(),
        }]);
        let (value, found) = secrets.lookup("p", "k").unwrap();
        assert_eq!(value, "sk-from-file");
        assert_eq!(
            found,
            Found::Rule {
                origin: "test".into(),
                source: Source::File(key),
            }
        );
        // Unnamed pairs fall to the convention variable, which is unset
        // here, so the answer is honestly nothing.
        assert!(secrets.lookup("p", "other").is_none());
        assert!(!secrets.is_available("p", "other"));
        assert!(
            secrets
                .describe_source("p", "other")
                .starts_with("environment variable GWENNOL_SECRET_P_OTHER")
        );
    }
}
