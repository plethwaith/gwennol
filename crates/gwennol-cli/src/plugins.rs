//! Finding and reading the plugin manifests to register.
//!
//! The kernel resolves nothing from disk: a plugin is one JSON document
//! with everything inline. The documents this frontend registers are
//! the output of `cargo xtask bundle` — the committed manifests with
//! their guest modules filled — read from a directory chosen by, in
//! order, `--plugins`, `$GWENNOL_PLUGINS`, the config file's
//! `[plugins] dir`, and finally `target/bundle/plugins` beside a
//! `cargo`-built binary (`target/<profile>/gwennol`). Shipping the
//! manifests inside the binary is distribution, which the roadmap
//! leaves past the MVP.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The subdirectory of a plugin tree that holds role contracts, which
/// `gwennol-core` registers itself.
const CONTRACTS_SUBDIR: &str = "spi";

/// One manifest, read but not yet registered.
#[derive(Debug, Clone)]
pub struct Plugin {
    /// The file.
    pub path: PathBuf,
    /// The document.
    pub manifest: Value,
}

impl Plugin {
    /// The manifest's `name`, or the file name when it has none (the
    /// kernel will refuse it with a better message than this can).
    pub fn name(&self) -> String {
        self.manifest
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// The secret names the manifest declares in `usesSecrets`.
    pub fn uses_secrets(&self) -> Vec<String> {
        self.manifest
            .get("usesSecrets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    }
}

/// Why the plugins could not be read.
#[derive(Debug, thiserror::Error)]
pub enum PluginsError {
    /// No directory was named and the cargo-layout default is absent.
    #[error(
        "no plugins directory: pass --plugins, set GWENNOL_PLUGINS, or set [plugins] dir in the \
         config file{looked}; `cargo xtask bundle` writes one under target/bundle/plugins"
    )]
    NoDir {
        /// The default that was tried, for the message.
        looked: String,
    },
    /// A file or directory could not be read.
    #[error("{path}: {source}")]
    Io {
        /// What.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// A manifest is not JSON.
    #[error("{path}: not JSON: {source}")]
    Json {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: serde_json::Error,
    },
}

/// Where the plugins directory was decided, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirOrigin {
    /// `--plugins` or `$GWENNOL_PLUGINS`.
    Flag,
    /// The config file.
    Config,
    /// `target/bundle/plugins` beside the binary.
    BesideBinary,
}

impl std::fmt::Display for DirOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Flag => "--plugins / GWENNOL_PLUGINS",
            Self::Config => "config file",
            Self::BesideBinary => "target/bundle beside the binary",
        })
    }
}

/// Choose the plugins directory. `flag` covers both the flag and the
/// environment variable (clap folds them); `config` is the config
/// file's entry, already resolved against the file's directory.
pub fn resolve_dir(
    flag: Option<PathBuf>,
    config: Option<PathBuf>,
) -> Result<(PathBuf, DirOrigin), PluginsError> {
    if let Some(dir) = flag {
        return Ok((dir, DirOrigin::Flag));
    }
    if let Some(dir) = config {
        return Ok((dir, DirOrigin::Config));
    }
    let beside = beside_binary();
    match &beside {
        Some(dir) if dir.is_dir() => Ok((dir.clone(), DirOrigin::BesideBinary)),
        _ => Err(PluginsError::NoDir {
            looked: beside
                .map(|d| format!(" (looked for {})", d.display()))
                .unwrap_or_default(),
        }),
    }
}

/// `target/bundle/plugins` for a binary at `target/<profile>/gwennol`.
fn beside_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.parent()?.join("bundle").join("plugins"))
}

/// Read every `*.json` under `dir`, recursively, in path order — the
/// order `cargo xtask bundle` wrote them, which puts `providers/`
/// before `tools/`. A `spi/` subdirectory is skipped: contracts are
/// not plugins, and the host has already registered its own.
pub fn load(dir: &Path) -> Result<Vec<Plugin>, PluginsError> {
    let mut paths = Vec::new();
    collect(dir, &mut paths)?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path).map_err(|source| PluginsError::Io {
                path: path.clone(),
                source,
            })?;
            let manifest = serde_json::from_str(&text).map_err(|source| PluginsError::Json {
                path: path.clone(),
                source,
            })?;
            Ok(Plugin { path, manifest })
        })
        .collect()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), PluginsError> {
    let io = |source| PluginsError::Io {
        path: dir.to_path_buf(),
        source,
    };
    for entry in std::fs::read_dir(dir).map_err(io)? {
        let path = entry.map_err(io)?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == CONTRACTS_SUBDIR) {
                tracing::debug!(dir = %path.display(), "skipping contracts directory");
                continue;
            }
            collect(&path, out)?;
        } else if path.extension().is_some_and(|x| x == "json") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_in_path_order_and_skips_contracts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (rel, text) in [
            ("tools/read.json", r#"{"name": "tool-read"}"#),
            (
                "providers/anthropic.json",
                r#"{"name": "provider-anthropic", "usesSecrets": ["api_key"]}"#,
            ),
            ("spi/tool.json", r#"{"role": "TOOL"}"#),
            ("providers/README.md", "not a manifest"),
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let plugins = load(root).unwrap();
        let names: Vec<_> = plugins.iter().map(Plugin::name).collect();
        assert_eq!(names, ["provider-anthropic", "tool-read"]);
        assert_eq!(plugins[0].uses_secrets(), ["api_key"]);
        assert!(plugins[1].uses_secrets().is_empty());
    }

    #[test]
    fn a_manifest_that_is_not_json_names_its_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.json"), "{").unwrap();
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("bad.json: not JSON"), "{err}");
    }

    #[test]
    fn the_flag_wins_over_the_config_over_the_default() {
        let (dir, origin) = resolve_dir(Some("/flag".into()), Some("/config".into())).unwrap();
        assert_eq!((dir, origin), (PathBuf::from("/flag"), DirOrigin::Flag));
        let (dir, origin) = resolve_dir(None, Some("/config".into())).unwrap();
        assert_eq!((dir, origin), (PathBuf::from("/config"), DirOrigin::Config));
        // Under `cargo test` the binary is at target/debug/deps/…, so
        // the cargo-layout default resolves to target/debug/bundle,
        // which does not exist: the error names what it looked for.
        match resolve_dir(None, None) {
            Err(PluginsError::NoDir { looked }) => {
                assert!(looked.contains("bundle"), "{looked}");
            }
            Ok((dir, origin)) => assert_eq!(origin, DirOrigin::BesideBinary, "{}", dir.display()),
            Err(other) => panic!("{other}"),
        }
    }
}
