//! The bundled SPI contracts and their names.
//!
//! Gwennol defines two roles: [`llm_chat`] for model providers and [`tool`]
//! for model-callable tools. The contract documents live in the repository
//! at `plugins/spi/` and are embedded here; [`crate::boot`] registers them
//! before any plugin can claim a role, because Gwead checks a plugin
//! against a role's contract only when the contract is already registered —
//! a claim on an unknown role loads with nothing but a warning.
//!
//! The wire shapes the contracts pin down are documented in `docs/SPI.md`.

use std::collections::BTreeSet;

/// The `LLM_CHAT` role: a chat-completion model provider.
pub mod llm_chat {
    /// The role name, as used in `roles` and `invoke:role:` permissions.
    pub const ROLE: &str = "LLM_CHAT";
    /// The one required action: messages in, assistant message (or stream
    /// handle) out.
    pub const CHAT: &str = "chat";
    /// The contract document, as shipped. The canonical file is
    /// `plugins/spi/llm_chat.json`; this embeds the crate's byte-identical
    /// copy (`cargo package` cannot reach outside the crate), pinned equal
    /// by a test below.
    pub const DEFINITION: &str = include_str!("../resources/spi/llm_chat.json");
}

/// The `TOOL` role: a plugin offering tools the model can call.
pub mod tool {
    /// The role name, as used in `roles` and `invoke:role:` permissions.
    pub const ROLE: &str = "TOOL";
    /// The one action the contract names, and it is optional: model-supplied
    /// arguments in, one uniform `{content, is_error, truncated}` result
    /// out. It is the name a plugin offering one tool conventionally gives
    /// its action. A tool is any action of a `TOOL` plugin that carries a
    /// `tool` block, whatever it is named (see
    /// [`super::harvest_tools`]).
    pub const CALL: &str = "call";
    /// The contract document, as shipped. Canonical file:
    /// `plugins/spi/tool.json`; see [`super::llm_chat::DEFINITION`].
    pub const DEFINITION: &str = include_str!("../resources/spi/tool.json");

    /// The one line appended to a truncated result's `content` before
    /// the model sees it — the shared truncation convention of
    /// `docs/SPI.md`. Tools report `truncated: true` as data and never
    /// compose this themselves; [`render_content`] is where it lands.
    pub const TRUNCATED_MARKER: &str = "[output truncated]";

    /// A tool result's `content` as the model should see it: verbatim,
    /// plus [`TRUNCATED_MARKER`] on its own final line when the tool
    /// reported a cut. The one implementation of the convention — the
    /// agent loop renders through here, and so do the tests that
    /// pin the bundled tools to it.
    pub fn render_content(content: &str, truncated: bool) -> String {
        if !truncated {
            return content.to_string();
        }
        let mut rendered = String::with_capacity(content.len() + TRUNCATED_MARKER.len() + 1);
        rendered.push_str(content);
        if !content.is_empty() && !content.ends_with('\n') {
            rendered.push('\n');
        }
        rendered.push_str(TRUNCATED_MARKER);
        rendered
    }
}

/// Every bundled contract as `(role, definition)`, in registration order.
///
/// Inventory, not a registration path: to put the contracts on a kernel,
/// call [`register`] rather than looping over this by hand.
pub const SPI_DEFINITIONS: [(&str, &str); 2] = [
    (llm_chat::ROLE, llm_chat::DEFINITION),
    (tool::ROLE, tool::DEFINITION),
];

/// Register every bundled contract with `kernel`.
///
/// Must run before any plugin claims a bundled role: Gwead checks a claim
/// against a contract only when the contract is already registered — an
/// unknown role loads with nothing but a warning. [`crate::boot_with`]
/// (and so [`crate::boot`]) calls this; any other registration path (a
/// test kernel, a future embedder seam) must call it too rather than
/// hand-rolling the loop.
pub fn register(kernel: &mut gwead::kernel::Kernel) -> Result<(), gwead::kernel::KernelError> {
    for (role, definition) in SPI_DEFINITIONS {
        kernel.register_spi_from_json(role, definition)?;
    }
    Ok(())
}

/// Why [`harvest_tools`] or [`harvest_selected`] refused the kernel's tool
/// inventory.
#[derive(Debug, thiserror::Error)]
pub enum HarvestError {
    /// Two offered tools carry the same `tool.name`, on two plugins or on
    /// two actions of one, so "the descriptor with that name" is ambiguous
    /// — and real provider APIs reject duplicate tools anyway.
    #[error("duplicate tool name {0:?}: more than one offered tool carries it")]
    DuplicateToolName(String),
    /// A selection named a plugin that offers no tool: not a `TOOL`
    /// fulfiller, or one none of whose actions carries a `tool` block.
    #[error("no TOOL plugin named {0:?} offers a tool")]
    NoSuchToolPlugin(String),
}

/// The tools the model may call, as harvested descriptors.
///
/// This is the one implementation of the harvest rules in `docs/SPI.md`
/// — Gwead's `get_tool_descriptors()` collects a `tool` block from *any*
/// action of *any* plugin, in unspecified order, so the raw list is not
/// what the model gets:
///
/// - only descriptors of a plugin that fulfils the [`tool::ROLE`] role
///   survive, one for each of its actions that carries a `tool` block; a
///   `tool` block on a plugin outside the role is never offered;
/// - when the caller selects plugins ([`harvest_selected`]), only those
///   plugins' descriptors survive, and the selection applies before the
///   duplicate refusal: two plugins that advertise one tool name can sit
///   on one kernel, and a selection holding at most one of them
///   harvests; [`harvest_tools`] on that kernel is refused;
/// - duplicate tool names are refused, not resolved, whether the two
///   tools are on two plugins or on one;
/// - the result is sorted by name, because tool order is model-visible
///   and prefix-sensitive: an unspecified order changes the prompt every
///   process start and busts provider-side prompt caching.
///
/// Each descriptor maps onto a `chat` `tools` entry as `tool_name →
/// name`, `description → description`, `parameters → input_schema`, and
/// its `plugin_key`/`action_name` say exactly what to execute when the
/// model names the tool.
///
/// Root-scoped, like `Kernel::execute_by_role`: fulfillers are resolved
/// from the root namespace, which is where Gwennol registers every
/// plugin — a `TOOL` plugin loaded into a non-root namespace would pass
/// its contract check yet not be harvested. That is the deliberate MVP
/// posture; namespaced deployments arrive with installable third-party
/// plugins ("Beyond the MVP" in the roadmap), and this signature grows a
/// namespace argument with them.
pub fn harvest_tools(
    kernel: &gwead::kernel::Kernel,
) -> Result<Vec<gwead::kernel::registry::ToolDescriptor>, HarvestError> {
    harvest(kernel, None)
}

/// [`harvest_tools`] restricted to the named plugins.
///
/// Selection is by plugin name, the unit a manifest grants authority to,
/// and applies before the duplicate-name refusal. A selected plugin brings
/// every tool it offers. A name that offers no tool (not a `TOOL`
/// fulfiller, or one none of whose actions carries a `tool` block) is
/// refused as [`HarvestError::NoSuchToolPlugin`], the first such name in
/// the set's order. An empty selection harvests nothing.
pub fn harvest_selected(
    kernel: &gwead::kernel::Kernel,
    plugins: &BTreeSet<String>,
) -> Result<Vec<gwead::kernel::registry::ToolDescriptor>, HarvestError> {
    harvest(kernel, Some(plugins))
}

fn harvest(
    kernel: &gwead::kernel::Kernel,
    selection: Option<&BTreeSet<String>>,
) -> Result<Vec<gwead::kernel::registry::ToolDescriptor>, HarvestError> {
    let fulfillers = kernel.role_candidates(None, tool::ROLE);
    let mut descriptors: Vec<_> = kernel
        .registry()
        .get_tool_descriptors()
        .into_iter()
        .filter(|d| fulfillers.contains(&d.plugin_key))
        .collect();
    if let Some(selection) = selection {
        if let Some(missing) = selection
            .iter()
            .find(|name| !descriptors.iter().any(|d| &d.plugin_key == *name))
        {
            return Err(HarvestError::NoSuchToolPlugin(missing.clone()));
        }
        descriptors.retain(|d| selection.contains(&d.plugin_key));
    }
    descriptors.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
    if let Some(pair) = descriptors
        .windows(2)
        .find(|pair| pair[0].tool_name == pair[1].tool_name)
    {
        return Err(HarvestError::DuplicateToolName(pair[0].tool_name.clone()));
    }
    Ok(descriptors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_result_shape_is_one_definition_and_call_is_optional() {
        let contract: serde_json::Value = serde_json::from_str(tool::DEFINITION).unwrap();
        let actions = contract["actions"].as_object().unwrap();
        assert_eq!(actions.keys().collect::<Vec<_>>(), ["call"]);
        assert_eq!(contract["actions"]["call"]["optional"], true);
        assert_eq!(
            contract["actions"]["call"]["output"],
            serde_json::json!({"$ref": "#/$defs/Result"})
        );
        let result = &contract["$defs"]["Result"];
        assert_eq!(result["required"], serde_json::json!(["content"]));
        assert_eq!(result["additionalProperties"], false);
        let mut properties: Vec<&String> =
            result["properties"].as_object().unwrap().keys().collect();
        properties.sort();
        assert_eq!(properties, ["content", "is_error", "truncated"]);
    }

    #[test]
    fn harvest_refusals_read_as_written() {
        assert_eq!(
            HarvestError::DuplicateToolName("x".into()).to_string(),
            "duplicate tool name \"x\": more than one offered tool carries it"
        );
        assert_eq!(
            HarvestError::NoSuchToolPlugin("x".into()).to_string(),
            "no TOOL plugin named \"x\" offers a tool"
        );
    }

    #[test]
    fn the_truncation_marker_lands_on_its_own_final_line() {
        use tool::{TRUNCATED_MARKER, render_content};
        assert_eq!(render_content("whole", false), "whole");
        assert_eq!(
            render_content("cut", true),
            format!("cut\n{TRUNCATED_MARKER}")
        );
        // A prefix that already ends a line is not given a blank one.
        assert_eq!(
            render_content("cut\n", true),
            format!("cut\n{TRUNCATED_MARKER}")
        );
        // Nothing kept at all still tells the model why.
        assert_eq!(render_content("", true), TRUNCATED_MARKER);
    }

    /// The name of a contract's canonical file under `plugins/spi/`,
    /// by convention: the role, lowercased.
    fn canonical_file(role: &str) -> String {
        format!("{}.json", role.to_lowercase())
    }

    /// The embedded copies must be byte-identical to the canonical
    /// documents in `plugins/spi/`. The one legitimate skip is the whole
    /// directory not existing (a packaged crate has only the copies) —
    /// any other failure to read it, permissions included, is an error.
    /// The file list derives from [`SPI_DEFINITIONS`] and is checked
    /// complete against the directory, so a third contract cannot land
    /// on either side without the other.
    ///
    /// Accepted residuals: a deliberate *rename* of `plugins/spi/` is
    /// indistinguishable from true absence and takes the skip (nothing
    /// short of a repo-layout oracle could tell them apart), and the
    /// completeness check sees only `*.json` — a contract accidentally
    /// saved under another extension sits outside it. Both are judged
    /// unlikely enough to live with.
    #[test]
    fn embedded_contracts_match_the_canonical_documents() {
        let canonical_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/spi");
        let entries = match std::fs::read_dir(&canonical_dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "skipping: no repository layout at {}",
                    canonical_dir.display()
                );
                return;
            }
            other => {
                other.unwrap_or_else(|e| panic!("cannot read {}: {e}", canonical_dir.display()))
            }
        };

        let on_disk: std::collections::BTreeSet<String> = entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".json"))
            .collect();
        let expected: std::collections::BTreeSet<String> = SPI_DEFINITIONS
            .iter()
            .map(|(role, _)| canonical_file(role))
            .collect();
        assert_eq!(
            on_disk, expected,
            "plugins/spi/*.json and SPI_DEFINITIONS disagree — a contract exists on one side only"
        );

        for (role, embedded) in SPI_DEFINITIONS {
            let path = canonical_dir.join(canonical_file(role));
            let canonical = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{role}: cannot read {}: {e}", path.display()));
            assert_eq!(
                embedded, canonical,
                "{role}: crates/gwennol-core/resources/spi/ has drifted from plugins/spi/ — copy the canonical file over"
            );
        }
    }
}
