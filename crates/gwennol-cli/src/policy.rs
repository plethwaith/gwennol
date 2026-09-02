//! The approval policy: ordered rules, first match wins, and a request
//! nothing matches is denied.
//!
//! The headless frontend has no prompt to fall back on, so every
//! [`Access`] a host step asks about is judged by rules alone, and every
//! judgement names the rule that made it — or says that none did. Rules
//! come from `--allow`/`--deny` flags and from `[[rules]]` tables in the
//! policy and config files; both spell a rule the same way, so a rule
//! tried on the command line moves into a file unchanged.
//!
//! # Rule grammar
//!
//! A rule is `<kind>:<pattern>`, where the kind names what the pattern
//! is matched against:
//!
//! | Kind    | Matched against                                        |
//! |---------|--------------------------------------------------------|
//! | `read`  | the canonical path of a file being read                |
//! | `write` | the path of a file being created or replaced           |
//! | `list`  | the canonical path of a directory being listed         |
//! | `spawn` | the argv of a process being spawned, joined by spaces  |
//! | `http`  | the full URL of an outbound request                    |
//! | `any`   | everything; takes no pattern                           |
//!
//! Patterns are globs. For the three path kinds, `*` does not cross a
//! `/` and `**` does, and a relative pattern is rooted at the workspace
//! — `read:**` is every file under the workspace, `read:/**` is every
//! file anywhere, `list:.` is the workspace root itself. For `spawn`
//! and `http` the pattern matches the whole subject and `*` matches
//! anything — `spawn:bash -c *`, `http:https://api.anthropic.com/*`.
//!
//! A rule from a file may also name a `plugin`; it then applies only to
//! requests from that plugin, so `write:**` can be granted to
//! `tool-write` and withheld from everything else.
//!
//! # Order
//!
//! Rules are tried in the order they were given — flags in command-line
//! order, then the policy file's rules, then the config file's — and
//! the first that matches decides. A request no rule matches is denied,
//! and the trace says so: there is no default that quietly allows.

use std::fmt;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use gwennol_core::{Access, ApprovalRequest, Decision};

/// What a rule matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// [`Access::ReadFile`].
    Read,
    /// [`Access::WriteFile`].
    Write,
    /// [`Access::ListDir`].
    List,
    /// [`Access::Spawn`].
    Spawn,
    /// [`Access::Http`].
    Http,
    /// Every access.
    Any,
}

impl Kind {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Self::Read,
            "write" => Self::Write,
            "list" => Self::List,
            "spawn" => Self::Spawn,
            "http" => Self::Http,
            "any" => Self::Any,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::List => "list",
            Self::Spawn => "spawn",
            Self::Http => "http",
            Self::Any => "any",
        }
    }

    /// The kind an access is judged under.
    fn of(access: &Access) -> Self {
        match access {
            Access::ReadFile(_) => Self::Read,
            Access::WriteFile(_) => Self::Write,
            Access::ListDir(_) => Self::List,
            Access::Spawn { .. } => Self::Spawn,
            Access::Http { .. } => Self::Http,
            // `Access` is non-exhaustive: a kind this frontend does not
            // know is one no rule of its can name, so it falls to the
            // default and is denied.
            _ => Self::Any,
        }
    }

    fn is_path(self) -> bool {
        matches!(self, Self::Read | Self::Write | Self::List)
    }
}

/// Where a rule came from, for the trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A `--allow` or `--deny` flag.
    Flag,
    /// The `index`-th (from 1) `[[rules]]` table of the file at `path`.
    File {
        /// The file.
        path: PathBuf,
        /// One-based position among that file's rules.
        index: usize,
    },
}

/// One rule as written, before the pattern is compiled against a
/// workspace: what flags and files produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSpec {
    /// Allow or deny.
    pub decision: Decision,
    /// The rule text, `<kind>:<pattern>` or `any`.
    pub text: String,
    /// Only requests from this plugin, when set.
    pub plugin: Option<String>,
    /// Where it was written.
    pub source: Source,
}

/// Why a rule could not be compiled.
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    /// The text before the colon is not a kind.
    #[error("rule {text:?}: unknown kind {kind:?} (read, write, list, spawn, http, any)")]
    UnknownKind {
        /// The rule.
        text: String,
        /// What was given.
        kind: String,
    },
    /// A kind that matches a subject was given nothing to match it with.
    #[error("rule {text:?}: {kind} needs a pattern, as {kind}:<glob>")]
    MissingPattern {
        /// The rule.
        text: String,
        /// The kind.
        kind: &'static str,
    },
    /// `any` was given a pattern.
    #[error("rule {text:?}: any takes no pattern")]
    AnyWithPattern {
        /// The rule.
        text: String,
    },
    /// The pattern is not a glob.
    #[error("rule {text:?}: {source}")]
    Glob {
        /// The rule.
        text: String,
        /// The glob compiler's reason.
        #[source]
        source: globset::Error,
    },
}

/// A compiled rule.
#[derive(Debug, Clone)]
pub struct Rule {
    spec: RuleSpec,
    kind: Kind,
    matcher: Option<GlobMatcher>,
}

impl Rule {
    /// Compile `spec` for a workspace rooted at `root`, which must be
    /// absolute: relative path patterns are rooted there.
    pub fn compile(spec: RuleSpec, root: &Path) -> Result<Self, RuleError> {
        let text = spec.text.clone();
        let (kind_text, pattern) = match text.split_once(':') {
            Some((k, p)) => (k, Some(p)),
            None => (text.as_str(), None),
        };
        let kind = Kind::parse(kind_text).ok_or_else(|| RuleError::UnknownKind {
            text: text.clone(),
            kind: kind_text.to_string(),
        })?;
        let matcher = match (kind, pattern) {
            (Kind::Any, None) => None,
            (Kind::Any, Some(_)) => return Err(RuleError::AnyWithPattern { text }),
            (kind, None) => {
                return Err(RuleError::MissingPattern {
                    text,
                    kind: kind.name(),
                });
            }
            (kind, Some(pattern)) if kind.is_path() => {
                let rooted = root_pattern(pattern, root);
                let glob = GlobBuilder::new(&rooted)
                    .literal_separator(true)
                    .build()
                    .map_err(|source| RuleError::Glob {
                        text: text.clone(),
                        source,
                    })?;
                Some(glob.compile_matcher())
            }
            (_, Some(pattern)) => {
                let glob = GlobBuilder::new(pattern)
                    .literal_separator(false)
                    .build()
                    .map_err(|source| RuleError::Glob {
                        text: text.clone(),
                        source,
                    })?;
                Some(glob.compile_matcher())
            }
        };
        Ok(Self {
            spec,
            kind,
            matcher,
        })
    }

    /// The rule as written.
    pub fn spec(&self) -> &RuleSpec {
        &self.spec
    }

    fn matches(&self, request: &ApprovalRequest) -> bool {
        if let Some(plugin) = &self.spec.plugin
            && plugin != &request.plugin
        {
            return false;
        }
        match self.kind {
            Kind::Any => true,
            kind if kind != Kind::of(&request.access) => false,
            _ => {
                let matcher = self.matcher.as_ref().expect("a non-any rule has a matcher");
                match &request.access {
                    Access::ReadFile(p) | Access::WriteFile(p) | Access::ListDir(p) => {
                        matcher.is_match(p)
                    }
                    Access::Spawn { argv, .. } => matcher.is_match(argv.join(" ")),
                    Access::Http { url, .. } => matcher.is_match(url),
                    _ => false,
                }
            }
        }
    }
}

/// A path pattern rooted at the workspace: an absolute pattern is
/// itself; `.` or an empty pattern is the root exactly; anything else is
/// joined under the root. The root's own characters are escaped so a
/// `[` or `*` in a directory name is not read as glob syntax.
fn root_pattern(pattern: &str, root: &Path) -> String {
    if Path::new(pattern).is_absolute() {
        return pattern.to_string();
    }
    let root = globset::escape(&root.to_string_lossy());
    match pattern {
        "" | "." => root,
        _ => format!("{root}/{pattern}"),
    }
}

/// The policy: every rule, in order.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    rules: Vec<Rule>,
}

/// A judgement on one request: the decision and the rule behind it.
#[derive(Debug, Clone)]
pub struct Judgement<'a> {
    /// The answer.
    pub decision: Decision,
    /// The rule that decided, or `None` for the default denial.
    pub rule: Option<&'a Rule>,
}

impl Policy {
    /// Compile `specs`, in order, for a workspace at `root`.
    pub fn compile(specs: Vec<RuleSpec>, root: &Path) -> Result<Self, RuleError> {
        let rules = specs
            .into_iter()
            .map(|spec| Rule::compile(spec, root))
            .collect::<Result<_, _>>()?;
        Ok(Self { rules })
    }

    /// The rules, in the order they are tried.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Judge a request: the first matching rule, or the default denial.
    pub fn judge(&self, request: &ApprovalRequest) -> Judgement<'_> {
        match self.rules.iter().find(|rule| rule.matches(request)) {
            Some(rule) => Judgement {
                decision: rule.spec.decision,
                rule: Some(rule),
            },
            None => Judgement {
                decision: Decision::Deny,
                rule: None,
            },
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flag => f.write_str("flag"),
            Self::File { path, index } => write!(f, "{} rule {index}", path.display()),
        }
    }
}

impl fmt::Display for RuleSpec {
    /// The rule as it would be written on the command line, then where
    /// it was actually written.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = match self.decision {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
        };
        match &self.source {
            Source::Flag => write!(f, "--{verb} {:?}", self.text)?,
            Source::File { .. } => write!(f, "{verb} {:?}", self.text)?,
        }
        if let Some(plugin) = &self.plugin {
            write!(f, " for plugin {plugin}")?;
        }
        if let Source::File { .. } = self.source {
            write!(f, " ({})", self.source)?;
        }
        Ok(())
    }
}

impl fmt::Display for Judgement<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.decision, self.rule) {
            (Decision::Allow, Some(rule)) => write!(f, "allowed by {}", rule.spec),
            (Decision::Deny, Some(rule)) => write!(f, "denied by {}", rule.spec),
            (_, None) => f.write_str("denied: no rule matched"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(decision: Decision, text: &str) -> RuleSpec {
        RuleSpec {
            decision,
            text: text.to_string(),
            plugin: None,
            source: Source::Flag,
        }
    }

    fn policy(specs: Vec<RuleSpec>) -> Policy {
        Policy::compile(specs, Path::new("/ws")).unwrap()
    }

    fn request(plugin: &str, access: Access) -> ApprovalRequest {
        ApprovalRequest {
            plugin: plugin.to_string(),
            cause: None,
            access,
        }
    }

    fn read(path: &str) -> ApprovalRequest {
        request("tool-read", Access::ReadFile(PathBuf::from(path)))
    }

    fn spawn(argv: &[&str]) -> ApprovalRequest {
        request(
            "tool-bash",
            Access::Spawn {
                argv: argv.iter().map(|s| s.to_string()).collect(),
                cwd: PathBuf::from("/ws"),
                stdin: None,
            },
        )
    }

    fn http(url: &str) -> ApprovalRequest {
        request(
            "provider-anthropic",
            Access::Http {
                method: "POST".into(),
                url: url.into(),
            },
        )
    }

    #[test]
    fn nothing_matching_is_denied_and_says_so() {
        let p = policy(vec![]);
        let j = p.judge(&read("/ws/a.txt"));
        assert_eq!(j.decision, Decision::Deny);
        assert!(j.rule.is_none());
        assert_eq!(j.to_string(), "denied: no rule matched");
    }

    #[test]
    fn relative_path_patterns_are_rooted_at_the_workspace() {
        let p = policy(vec![flag(Decision::Allow, "read:**")]);
        assert_eq!(p.judge(&read("/ws/a.txt")).decision, Decision::Allow);
        assert_eq!(
            p.judge(&read("/ws/deep/er/a.txt")).decision,
            Decision::Allow
        );
        assert_eq!(p.judge(&read("/etc/passwd")).decision, Decision::Deny);
        // A sibling whose name merely starts with the root's.
        assert_eq!(p.judge(&read("/ws2/a.txt")).decision, Decision::Deny);
    }

    #[test]
    fn a_single_star_stays_within_one_path_component() {
        let p = policy(vec![flag(Decision::Allow, "read:src/*.rs")]);
        assert_eq!(p.judge(&read("/ws/src/main.rs")).decision, Decision::Allow);
        assert_eq!(
            p.judge(&read("/ws/src/agent/mod.rs")).decision,
            Decision::Deny
        );
        assert_eq!(p.judge(&read("/ws/main.rs")).decision, Decision::Deny);
    }

    #[test]
    fn absolute_patterns_and_the_root_itself() {
        let p = policy(vec![
            flag(Decision::Allow, "read:/**"),
            flag(Decision::Allow, "list:."),
        ]);
        assert_eq!(p.judge(&read("/etc/hosts")).decision, Decision::Allow);
        let root = request("tool-list", Access::ListDir(PathBuf::from("/ws")));
        assert_eq!(p.judge(&root).decision, Decision::Allow);
        let sub = request("tool-list", Access::ListDir(PathBuf::from("/ws/src")));
        assert_eq!(p.judge(&sub).decision, Decision::Deny);
    }

    #[test]
    fn glob_syntax_in_the_workspace_path_is_literal() {
        let root = Path::new("/ws[1]/*star");
        let p = Policy::compile(vec![flag(Decision::Allow, "read:**")], root).unwrap();
        assert_eq!(
            p.judge(&read("/ws[1]/*star/a.txt")).decision,
            Decision::Allow
        );
        assert_eq!(p.judge(&read("/ws1/xstar/a.txt")).decision, Decision::Deny);
    }

    #[test]
    fn kinds_do_not_cross() {
        let p = policy(vec![flag(Decision::Allow, "read:**")]);
        let write = request("tool-write", Access::WriteFile(PathBuf::from("/ws/a.txt")));
        assert_eq!(p.judge(&write).decision, Decision::Deny);
    }

    #[test]
    fn spawn_matches_the_joined_argv_and_star_crosses_everything() {
        let p = policy(vec![flag(Decision::Allow, "spawn:bash -c cargo *")]);
        assert_eq!(
            p.judge(&spawn(&["bash", "-c", "cargo test --workspace"]))
                .decision,
            Decision::Allow
        );
        assert_eq!(
            p.judge(&spawn(&["bash", "-c", "cargo build src/x"]))
                .decision,
            Decision::Allow,
            "a slash in an argument is not a separator for spawn"
        );
        assert_eq!(
            p.judge(&spawn(&["bash", "-c", "rm -rf build"])).decision,
            Decision::Deny
        );
        assert_eq!(p.judge(&spawn(&["bash"])).decision, Decision::Deny);
    }

    #[test]
    fn http_matches_the_whole_url() {
        let p = policy(vec![flag(
            Decision::Allow,
            "http:https://api.anthropic.com/*",
        )]);
        assert_eq!(
            p.judge(&http("https://api.anthropic.com/v1/messages"))
                .decision,
            Decision::Allow
        );
        assert_eq!(
            p.judge(&http("https://api.anthropic.com.evil.example/v1/messages"))
                .decision,
            Decision::Deny
        );
        assert_eq!(
            p.judge(&http("http://api.anthropic.com/v1/messages"))
                .decision,
            Decision::Deny
        );
    }

    #[test]
    fn first_match_wins_in_the_order_given() {
        let p = policy(vec![
            flag(Decision::Deny, "write:.git/**"),
            flag(Decision::Allow, "write:**"),
        ]);
        let hook = request(
            "tool-write",
            Access::WriteFile(PathBuf::from("/ws/.git/hooks/pre-commit")),
        );
        let j = p.judge(&hook);
        assert_eq!(j.decision, Decision::Deny);
        assert_eq!(j.to_string(), r#"denied by --deny "write:.git/**""#);
        let src = request(
            "tool-write",
            Access::WriteFile(PathBuf::from("/ws/src/a.rs")),
        );
        let j = p.judge(&src);
        assert_eq!(j.decision, Decision::Allow);
        assert_eq!(j.to_string(), r#"allowed by --allow "write:**""#);

        // The same two rules the other way round: the broad allow
        // shadows the narrow deny, exactly as written.
        let p = policy(vec![
            flag(Decision::Allow, "write:**"),
            flag(Decision::Deny, "write:.git/**"),
        ]);
        assert_eq!(p.judge(&hook).decision, Decision::Allow);
    }

    #[test]
    fn any_matches_every_kind_and_a_plugin_qualifier_narrows() {
        let file = |decision, text: &str, plugin: Option<&str>, index| RuleSpec {
            decision,
            text: text.to_string(),
            plugin: plugin.map(str::to_string),
            source: Source::File {
                path: PathBuf::from("policy.toml"),
                index,
            },
        };
        let p = policy(vec![
            file(Decision::Allow, "any", Some("provider-anthropic"), 1),
            file(Decision::Allow, "spawn:*", Some("tool-bash"), 2),
        ]);
        let j = p.judge(&http("https://api.anthropic.com/v1/messages"));
        assert_eq!(j.decision, Decision::Allow);
        assert_eq!(
            j.to_string(),
            r#"allowed by allow "any" for plugin provider-anthropic (policy.toml rule 1)"#
        );
        // `any` for the provider does not reach the tools.
        assert_eq!(p.judge(&read("/ws/a.txt")).decision, Decision::Deny);
        assert_eq!(
            p.judge(&spawn(&["bash", "-c", "ls"])).decision,
            Decision::Allow
        );
        let other = request(
            "tool-grep",
            Access::Spawn {
                argv: vec!["grep".into()],
                cwd: PathBuf::from("/ws"),
                stdin: None,
            },
        );
        assert_eq!(p.judge(&other).decision, Decision::Deny);
    }

    #[test]
    fn malformed_rules_are_refused_with_the_reason() {
        let err = |text: &str| {
            Rule::compile(flag(Decision::Allow, text), Path::new("/ws"))
                .unwrap_err()
                .to_string()
        };
        assert!(err("open:**").contains("unknown kind \"open\""));
        assert!(err("read").contains("read needs a pattern"));
        assert!(err("any:x").contains("any takes no pattern"));
        assert!(err("read:[").contains("rule \"read:[\""));
    }
}
