//! Process-global host state the native step bodies read.
//!
//! Gwead native step implementations are bare `fn` pointers collected at
//! link time, so they cannot capture an [`Operator`]. The operator and the
//! workspace root are process-wide singletons anyway — one session, one
//! frontend — so a `OnceLock` matches their real lifetime: set once by
//! [`crate::boot`], read by every step invocation afterwards.
//!
//! Tests in one binary therefore share a single host. The integration tests
//! install a recording operator once and inspect what it was asked.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gwead::kernel::PluginExecution;

use crate::context::tool_call;
use crate::operator::{Access, ApprovalRequest, Decision, Operator};

/// Variables a spawned child inherits under [`ProcessEnv::AllowList`] by
/// default: enough to resolve a program, find a home directory and decode
/// text, and nothing that carries authority.
pub const DEFAULT_ENV_ALLOWLIST: &[&str] = &[
    "HOME", "LANG", "LC_ALL", "LC_CTYPE", "LOGNAME", "PATH", "SHELL", "TERM", "TMPDIR", "TZ",
    "USER",
];

/// What environment `host_process.run` gives a child.
///
/// Policy, so the frontend chooses it; the host never widens what it is
/// given. The default is an allow-list rather than inheritance because a
/// coding agent is started from exactly the shell that holds the user's API
/// keys, `SSH_AUTH_SOCK`, and cloud credentials — authority no plugin asked
/// for, that no manifest declares, and that the operator never saw in an
/// approval.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessEnv {
    /// Pass through these variables of the host's environment, when set.
    AllowList(Vec<String>),
    /// Give the child exactly this environment, whatever the host's holds.
    Fixed(BTreeMap<String, String>),
    /// Inherit the host process's environment wholesale. An explicit
    /// choice, never a default: see the type docs for what that hands over.
    Inherit,
}

impl Default for ProcessEnv {
    fn default() -> Self {
        Self::AllowList(
            DEFAULT_ENV_ALLOWLIST
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    }
}

impl ProcessEnv {
    /// The concrete environment to install, or `None` to inherit.
    pub fn resolve(&self) -> Option<BTreeMap<String, OsString>> {
        match self {
            Self::Inherit => None,
            Self::AllowList(names) => Some(
                names
                    .iter()
                    .filter_map(|n| std::env::var_os(n).map(|v| (n.clone(), v)))
                    .collect(),
            ),
            Self::Fixed(vars) => Some(
                vars.iter()
                    .map(|(k, v)| (k.clone(), OsString::from(v)))
                    .collect(),
            ),
        }
    }
}

/// What the host needs to know before any step runs.
pub struct HostConfig {
    /// The frontend.
    pub operator: Arc<dyn Operator>,
    /// Directory relative paths resolve against, and the default working
    /// directory for spawned processes. Stored as given; the operator sees
    /// absolute, lexically-normalised paths built from it.
    pub workspace_root: PathBuf,
    /// Environment policy for `host_process.run`.
    pub process_env: ProcessEnv,
}

static HOST: OnceLock<HostConfig> = OnceLock::new();

/// Install the host state. Returns `Err` with the rejected config if one
/// was already installed; the first one stays authoritative.
pub fn install(config: HostConfig) -> Result<(), HostConfig> {
    HOST.set(config)
}

/// The installed host state.
///
/// # Panics
///
/// If nothing was installed. Every step body is reached through a kernel
/// that [`crate::boot`] built after installing, so this is unreachable in
/// normal operation and a programming error otherwise.
pub fn host() -> &'static HostConfig {
    HOST.get().expect(
        "gwennol host state not installed — call gwennol_core::boot before executing actions",
    )
}

/// Resolve a plugin-supplied path to the absolute, lexically-normalised
/// form the operator is shown.
///
/// Relative paths join the workspace root. `.` and `..` components are
/// folded without touching the filesystem, so the operator judges the path
/// the plugin named rather than wherever a symlink might lead — resolving
/// symlinks is a policy choice left to the operator.
pub fn resolve_path(raw: &str) -> PathBuf {
    let joined = {
        let p = Path::new(raw);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            host().workspace_root.join(p)
        }
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Describe what the executing plugin wants, ready for [`approve`].
///
/// Split from the asking so the borrow of the execution ends before the
/// step body awaits: the request owns everything the operator is shown.
pub fn approval(ex: &(dyn PluginExecution + Send), access: Access) -> ApprovalRequest {
    ApprovalRequest {
        plugin: ex.plugin_name().to_string(),
        cause: tool_call(ex.exec_ctx()),
        access,
    }
}

/// A payload-free description of an [`Access`] for error messages. The
/// denial travels back into plugin-visible errors and logs, so it must
/// not carry stdin, header values, or argv beyond the program — any of
/// which can hold interpolated secrets.
fn describe(access: &Access) -> String {
    match access {
        Access::ReadFile(p) => format!("read of {}", p.display()),
        Access::WriteFile(p) => format!("write to {}", p.display()),
        Access::ListDir(p) => format!("listing of {}", p.display()),
        Access::Spawn { argv, .. } => format!(
            "spawn of {:?}",
            argv.first().map(String::as_str).unwrap_or("?")
        ),
        Access::Http { method, url } => format!("{method} {url}"),
    }
}

/// Ask the operator. `Err` carries a message suitable for a `StepError`.
pub async fn approve(request: ApprovalRequest) -> Result<(), String> {
    let describe = describe(&request.access);
    let plugin = request.plugin.clone();
    match host().operator.approve(request).await {
        Decision::Allow => Ok(()),
        Decision::Deny => Err(format!("operator denied {describe} for plugin '{plugin}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_passes_what_is_set_and_invents_nothing() {
        let env = ProcessEnv::AllowList(vec!["PATH".into(), "GWENNOL_NOT_SET".into()])
            .resolve()
            .expect("not inheriting");
        assert!(env.contains_key("PATH"));
        assert!(!env.contains_key("GWENNOL_NOT_SET"));
    }

    #[test]
    fn the_default_withholds_credentials_the_agent_was_launched_with() {
        let default = ProcessEnv::default().resolve().expect("not inheriting");
        for name in [
            "ANTHROPIC_API_KEY",
            "SSH_AUTH_SOCK",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(!default.contains_key(name), "{name} is allow-listed");
        }
        assert!(
            DEFAULT_ENV_ALLOWLIST.contains(&"PATH"),
            "a child needs PATH"
        );
    }

    #[test]
    fn fixed_is_exactly_what_it_says_and_inherit_defers() {
        let vars = BTreeMap::from([("ONLY".to_string(), "this".to_string())]);
        let env = ProcessEnv::Fixed(vars).resolve().expect("not inheriting");
        assert_eq!(env.len(), 1);
        assert_eq!(env["ONLY"], OsString::from("this"));
        assert_eq!(ProcessEnv::Inherit.resolve(), None);
    }
}
