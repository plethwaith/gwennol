//! The resolution context an entry point receives.

use serde_json::Value;

/// The step's resolution context, as Gwead serializes it for a script
/// step.
///
/// The layout is the kernel's, not this crate's: the **action input's
/// top-level fields are flattened at the root**, alongside the
/// namespace keys the kernel adds — and the kernel's keys always win.
/// Four are always present (`config`, `secrets`, `vars`, `steps`) and
/// three appear conditionally (`item` inside a `for_each`/`repeat`
/// body, `error` inside a `try` step's catch, `trigger` for
/// event-dispatched actions), so an input field named `error` would be
/// shadowed only sometimes — the worst way. Contracts for guest-backed
/// actions should avoid all seven names.
///
/// `secrets` holds only the keys the step's `passSecrets` allowlist
/// names — absent `passSecrets` means an empty object, by the kernel's
/// design, so a guest that needs a credential and reads `None` here
/// should suspect the manifest before the environment.
#[derive(Debug, Clone)]
pub struct Args {
    root: Value,
}

impl Args {
    pub(crate) fn new(root: Value) -> Self {
        Args { root }
    }

    /// The whole context, untyped.
    pub fn raw(&self) -> &Value {
        &self.root
    }

    /// The whole context, by value — for entry points that deserialize
    /// into their own types.
    pub fn into_raw(self) -> Value {
        self.root
    }

    /// A top-level field: an action input field, or one of the
    /// kernel's namespace keys.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.root.get(name)
    }

    /// The plugin's `config` namespace (`Value::Null` when absent).
    pub fn config(&self) -> &Value {
        self.root.get("config").unwrap_or(&Value::Null)
    }

    /// A secret the step's `passSecrets` allowlist admitted. Gwead
    /// resolves secrets to strings, so the accessor is typed as one.
    pub fn secret(&self, name: &str) -> Option<&str> {
        self.root.get("secrets")?.get(name)?.as_str()
    }

    /// A prior step's primary result: `steps.<id>.result`.
    pub fn step_result(&self, step_id: &str) -> Option<&Value> {
        self.root.get("steps")?.get(step_id)?.get("result")
    }

    /// A prior step's sidecar metadata field (`steps.<id>.<key>`), such
    /// as an HTTP step's `status`.
    pub fn step_meta(&self, step_id: &str, key: &str) -> Option<&Value> {
        self.root.get("steps")?.get(step_id)?.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context() -> Args {
        Args::new(json!({
            "url": "http://example.invalid/sse",
            "config": {"model": "test-model"},
            "secrets": {"api_key": "sk-fixture"},
            "vars": {},
            "steps": {
                "fetch": {"status": 200, "result": {"body": 3}}
            }
        }))
    }

    #[test]
    fn accessors_reach_each_namespace() {
        let args = context();
        assert_eq!(
            args.field("url").and_then(Value::as_str),
            Some("http://example.invalid/sse")
        );
        assert_eq!(args.config()["model"], json!("test-model"));
        assert_eq!(args.secret("api_key"), Some("sk-fixture"));
        assert_eq!(args.step_result("fetch"), Some(&json!({"body": 3})));
        assert_eq!(args.step_meta("fetch", "status"), Some(&json!(200)));
    }

    #[test]
    fn absent_lookups_are_none_not_panics() {
        let args = Args::new(json!({}));
        assert!(args.field("url").is_none());
        assert!(args.config().is_null());
        assert!(args.secret("api_key").is_none());
        assert!(args.step_result("fetch").is_none());
    }
}
