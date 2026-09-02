//! Bundling guest-backed plugins: the build-time half of the plugin
//! substrate.
//!
//! A committed manifest under `plugins/` names a guest module by the
//! crate that builds it — `"wasmModules": {"guest": {"path":
//! "crates/<name>"}}` — a form the Gwead kernel refuses, so the file in
//! the repository is honestly *not* a registrable plugin and no
//! compiled blob is ever committed. [`bundle`] compiles each such crate
//! to `wasm32-unknown-unknown` and replaces the `path` form with the
//! inline `base64` form the kernel accepts. The JSON file stays the
//! plugin; the bundle is the same document with its one slot filled.
//!
//! `cargo xtask bundle` writes the result under `target/bundle/`; the
//! integration suites call [`bundle`] directly so the manifests they
//! test are the committed ones, filled by the same code.
//!
//! Conventions this relies on, all checked rather than assumed: a guest
//! crate's directory name is its package name (the artifact is looked
//! up by that name), and the guest is built with the same `cargo` that
//! runs the task, into its own target directory so an outer
//! `cargo test`'s lock and the guest build's lock never meet.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// The plugin manifests, relative to the workspace root.
pub const PLUGINS_DIR: &str = "plugins";

/// The subdirectory of [`PLUGINS_DIR`] holding role contracts, which
/// are not plugins and are bundled by `gwennol-core` itself.
pub const CONTRACTS_SUBDIR: &str = "spi";

/// Where guest crates are built, relative to the workspace root —
/// separate from `target/` so a nested build never contends for the
/// outer build's lock.
pub const GUEST_TARGET_DIR: &str = "target/wasm-guest";

/// Where `cargo xtask bundle` writes by default, relative to the
/// workspace root.
pub const BUNDLE_DIR: &str = "target/bundle";

/// The guest target triple.
pub const WASM_TARGET: &str = "wasm32-unknown-unknown";

/// Why bundling failed.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// A file or directory could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// What was being accessed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A manifest is not JSON.
    #[error("{path}: not JSON: {source}")]
    Json {
        /// The manifest.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: serde_json::Error,
    },
    /// A manifest's `wasmModules` entry is neither form.
    #[error("{path}: wasmModules.{module} must be {{\"path\": …}} or {{\"base64\": …}}")]
    ModuleShape {
        /// The manifest.
        path: PathBuf,
        /// The module key.
        module: String,
    },
    /// `cargo build` for a guest crate failed.
    #[error(
        "building {crate_dir} for {WASM_TARGET} failed (is the target installed? \
         `rustup target add {WASM_TARGET}`):\n{stderr}"
    )]
    Build {
        /// The crate directory, relative to the workspace.
        crate_dir: PathBuf,
        /// What cargo said.
        stderr: String,
    },
}

/// The workspace root this crate was built from.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the xtask crate sits two levels below the workspace root")
}

/// The cargo running this process, or `cargo` off the path.
fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// Compile the guest crate at `crate_dir` (relative to `workspace`) to
/// wasm and return the module bytes.
pub fn build_guest(workspace: &Path, crate_dir: &Path) -> Result<Vec<u8>, BundleError> {
    let package = crate_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target_dir = workspace.join(GUEST_TARGET_DIR);
    let output = Command::new(cargo())
        .current_dir(workspace)
        .args([
            "build",
            "-p",
            &package,
            "--target",
            WASM_TARGET,
            "--release",
            "--locked",
        ])
        .arg("--target-dir")
        .arg(&target_dir)
        .output()
        .map_err(|source| BundleError::Io {
            path: PathBuf::from(cargo()),
            source,
        })?;
    if !output.status.success() {
        return Err(BundleError::Build {
            crate_dir: crate_dir.to_path_buf(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    // cdylib artifacts are named after the package with `-` as `_`.
    let artifact = target_dir
        .join(WASM_TARGET)
        .join("release")
        .join(format!("{}.wasm", package.replace('-', "_")));
    std::fs::read(&artifact).map_err(|source| BundleError::Io {
        path: artifact,
        source,
    })
}

/// Fill every `{"path": …}` module in `manifest` with the compiled
/// bytes of the crate it names. Returns the crate directories built.
/// A module already in `base64` form is left alone.
pub fn inject_guests(
    manifest: &mut Value,
    manifest_path: &Path,
    workspace: &Path,
) -> Result<Vec<PathBuf>, BundleError> {
    use base64::Engine as _;
    let mut built = Vec::new();
    let Some(Value::Object(modules)) = manifest.get_mut("wasmModules") else {
        return Ok(built);
    };
    for (key, module) in modules.iter_mut() {
        let crate_dir = match module {
            Value::Object(m) if m.contains_key("base64") => continue,
            Value::Object(m) => match m.get("path").and_then(Value::as_str) {
                Some(p) => PathBuf::from(p),
                None => {
                    return Err(BundleError::ModuleShape {
                        path: manifest_path.to_path_buf(),
                        module: key.clone(),
                    });
                }
            },
            _ => {
                return Err(BundleError::ModuleShape {
                    path: manifest_path.to_path_buf(),
                    module: key.clone(),
                });
            }
        };
        let bytes = build_guest(workspace, &crate_dir)?;
        *module = serde_json::json!({
            "base64": base64::engine::general_purpose::STANDARD.encode(bytes)
        });
        built.push(crate_dir);
    }
    Ok(built)
}

/// One bundled plugin: the committed manifest with its guest slots
/// filled.
#[derive(Debug, Clone)]
pub struct BundledPlugin {
    /// The manifest's path relative to the workspace root, e.g.
    /// `plugins/providers/anthropic.json`.
    pub relative_path: PathBuf,
    /// The registrable document.
    pub manifest: Value,
    /// Guest crate directories compiled into it (empty for a
    /// declarative plugin).
    pub guests: Vec<PathBuf>,
}

impl BundledPlugin {
    /// The manifest's `name`.
    pub fn name(&self) -> &str {
        self.manifest
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    /// The manifest's subdirectory under [`PLUGINS_DIR`] (`providers`,
    /// `tools`).
    pub fn group(&self) -> &str {
        self.relative_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .unwrap_or("")
    }
}

/// Bundle every plugin manifest under `plugins/` (the `spi/` contracts
/// excepted), in path order, compiling guest crates as needed.
pub fn bundle(workspace: &Path) -> Result<Vec<BundledPlugin>, BundleError> {
    let plugins_dir = workspace.join(PLUGINS_DIR);
    let io = |path: &Path, source| BundleError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut manifests = Vec::new();
    for group in std::fs::read_dir(&plugins_dir).map_err(|e| io(&plugins_dir, e))? {
        let group = group.map_err(|e| io(&plugins_dir, e))?.path();
        if !group.is_dir() || group.file_name().is_some_and(|n| n == CONTRACTS_SUBDIR) {
            continue;
        }
        for entry in std::fs::read_dir(&group).map_err(|e| io(&group, e))? {
            let path = entry.map_err(|e| io(&group, e))?.path();
            if path.extension().is_some_and(|x| x == "json") {
                manifests.push(path);
            }
        }
    }
    manifests.sort();
    let mut bundled = Vec::new();
    for path in manifests {
        let text = std::fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        let mut manifest: Value =
            serde_json::from_str(&text).map_err(|source| BundleError::Json {
                path: path.clone(),
                source,
            })?;
        let relative_path = path
            .strip_prefix(workspace)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.clone());
        let guests = inject_guests(&mut manifest, &relative_path, workspace)?;
        bundled.push(BundledPlugin {
            relative_path,
            manifest,
            guests,
        });
    }
    Ok(bundled)
}

/// Write bundled manifests under `out_dir`, mirroring their layout
/// under `plugins/`. Returns the files written.
pub fn write_bundle(
    bundled: &[BundledPlugin],
    out_dir: &Path,
) -> Result<Vec<PathBuf>, BundleError> {
    let mut written = Vec::new();
    for plugin in bundled {
        let dest = out_dir.join(&plugin.relative_path);
        let io = |source| BundleError::Io {
            path: dest.clone(),
            source,
        };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let text = serde_json::to_string_pretty(&plugin.manifest).expect("a Value serialises");
        std::fs::write(&dest, text).map_err(io)?;
        written.push(dest);
    }
    Ok(written)
}
