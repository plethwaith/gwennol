//! The bundled SPI contracts and their names.
//!
//! Gwennol defines two roles: [`llm_chat`] for model providers and [`tool`]
//! for model-callable tools. The contract documents live in the repository
//! at `plugins/spi/` and are embedded here; [`register`] registers them,
//! and must run before any plugin claims a role, because Gwead checks a plugin
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
    /// The optional action: what the provider takes under its `$config`.
    /// A provider may omit it.
    pub const ACCEPTS: &str = "accepts";
    /// The contract document, as shipped. The canonical file is
    /// `plugins/spi/llm_chat.json`; this embeds the crate's byte-identical
    /// copy (`cargo package` cannot reach outside the crate), pinned equal
    /// by a test below.
    pub const DEFINITION: &str = include_str!("../resources/spi/llm_chat.json");
}

/// The `TOOL` role: a plugin offering tools the model can call.
pub mod tool {
    use crate::content::{ToolContent, UserContent};

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

    /// The marker added to a truncated result's `content` before
    /// the model sees it — the shared truncation convention of
    /// `docs/SPI.md`. Tools report `truncated: true` as data and never
    /// compose this themselves; [`render_content`] is where it lands on a
    /// text result, and on a list it is a text block of its own, last.
    pub const TRUNCATED_MARKER: &str = "[output truncated]";

    /// A text tool result's `content` as the model should see it: verbatim,
    /// plus [`TRUNCATED_MARKER`] on its own final line when the tool
    /// reported a cut. The one implementation of the convention for text:
    /// the agent loop renders a text result through here (by `render`),
    /// and so do the tests that pin the bundled tools to it.
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

    /// A tool result's content as the model should see it:
    /// [`render_content`] for text, and for a list the marker as a text
    /// block of its own after the last block, when the tool reported a cut.
    pub(crate) fn render(content: ToolContent, truncated: bool) -> ToolContent {
        match content {
            ToolContent::Text(text) => ToolContent::Text(render_content(&text, truncated)),
            ToolContent::Blocks(mut items) => {
                if truncated {
                    items.push(UserContent::Text(TRUNCATED_MARKER.to_string()));
                }
                ToolContent::Blocks(items)
            }
        }
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
/// unknown role loads with nothing but a warning. `gwennol_host::boot_with`
/// calls it; any other registration path (a test kernel, an embedder's own
/// kernel) must call it too rather than hand-rolling the loop.
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
    /// A selection held a name that offers no tool.
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
/// every tool it offers. A name that offers no tool is refused as
/// [`HarvestError::NoSuchToolPlugin`], the first such name in the set's
/// order. An empty selection harvests nothing.
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
        assert_eq!(contract["version"], "0.4.0");
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
    fn a_user_message_admits_an_image_block() {
        use crate::content::ImageFormat;
        use serde_json::json;
        let contract: serde_json::Value = serde_json::from_str(llm_chat::DEFINITION).unwrap();
        assert_eq!(contract["version"], "0.7.0");
        let defs = &contract["$defs"];
        assert_eq!(
            defs["UserMessage"]["properties"]["content"]["items"]["oneOf"],
            json!([
                {"$ref": "#/$defs/TextBlock"},
                {"$ref": "#/$defs/ToolResultBlock"},
                {"$ref": "#/$defs/ImageBlock"}
            ])
        );
        assert_eq!(defs["ImageBlock"]["additionalProperties"], false);
        assert_eq!(defs["ImageBlock"]["required"], json!(["type", "source"]));
        assert_eq!(
            defs["ImageBlock"]["properties"]["source"],
            json!({"oneOf": [
                {"$ref": "#/$defs/Base64ImageSource"},
                {"$ref": "#/$defs/ReferenceImageSource"}
            ]})
        );
        let source = &defs["Base64ImageSource"];
        assert_eq!(source["additionalProperties"], false);
        assert_eq!(source["required"], json!(["type", "media_type", "data"]));
        let reference = &defs["ReferenceImageSource"];
        assert_eq!(reference["additionalProperties"], false);
        assert_eq!(
            reference["required"],
            json!(["type", "media_type", "reference"])
        );
        for source in [source, reference] {
            assert_eq!(
                source["properties"]["media_type"],
                json!({"$ref": "#/$defs/ImageMediaType"})
            );
        }
        assert_eq!(
            defs["ImageMediaType"]["enum"],
            json!(
                ImageFormat::ALL
                    .iter()
                    .map(|f| f.media_type())
                    .collect::<Vec<_>>()
            )
        );
    }

    #[test]
    fn a_provider_may_declare_what_it_accepts() {
        use crate::accepts::ImageSource;
        use serde_json::json;
        let contract: serde_json::Value = serde_json::from_str(llm_chat::DEFINITION).unwrap();
        assert_eq!(contract["version"], "0.7.0");
        let accepts = &contract["actions"][llm_chat::ACCEPTS];
        assert_eq!(accepts["optional"], true);
        assert_eq!(
            accepts["input"],
            json!({"type": "object", "additionalProperties": false, "properties": {}})
        );
        assert_eq!(accepts["output"]["type"], "object");
        assert_eq!(accepts["output"]["additionalProperties"], true);
        assert_eq!(
            accepts["output"]["properties"]["images"]["oneOf"],
            json!([{"const": false}, {"$ref": "#/$defs/ImageInput"}])
        );
        let defs = &contract["$defs"];
        let input = &defs["ImageInput"];
        assert_eq!(input["required"], json!(["media_types", "sources"]));
        assert_eq!(input["additionalProperties"], true);
        for field in ["media_types", "sources"] {
            let array = &input["properties"][field];
            assert_eq!(array["type"], "array", "{field}");
            assert_eq!(array["minItems"], 1, "{field}");
            assert_eq!(array["uniqueItems"], true, "{field}");
        }
        assert_eq!(
            input["properties"]["media_types"]["items"],
            json!({"$ref": "#/$defs/ImageMediaType"})
        );
        let sources = &input["properties"]["sources"]["items"]["enum"];
        assert_eq!(
            sources,
            &json!([
                defs["Base64ImageSource"]["properties"]["type"]["const"],
                defs["ReferenceImageSource"]["properties"]["type"]["const"],
            ])
        );
        assert_eq!(
            sources,
            &json!(
                ImageSource::ALL
                    .iter()
                    .map(|s| s.source_type())
                    .collect::<Vec<_>>()
            )
        );
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
    fn a_tool_result_may_carry_text_and_image_blocks() {
        use serde_json::{Value, json};
        let tool: Value = serde_json::from_str(tool::DEFINITION).unwrap();
        let chat: Value = serde_json::from_str(llm_chat::DEFINITION).unwrap();
        assert_eq!(tool["version"], "0.4.0");
        assert_eq!(chat["version"], "0.7.0");

        fn without_descriptions(value: &Value) -> Value {
            match value {
                Value::Object(map) => Value::Object(
                    map.iter()
                        .filter(|(key, _)| key.as_str() != "description")
                        .map(|(key, value)| (key.clone(), without_descriptions(value)))
                        .collect(),
                ),
                Value::Array(items) => {
                    Value::Array(items.iter().map(without_descriptions).collect())
                }
                other => other.clone(),
            }
        }
        let one_of = json!({"oneOf": [
            {"type": "string"},
            {"type": "array", "minItems": 1, "items": {"oneOf": [
                {"$ref": "#/$defs/TextBlock"},
                {"$ref": "#/$defs/ImageBlock"}
            ]}}
        ]});
        assert_eq!(
            without_descriptions(&tool["$defs"]["Result"]["properties"]["content"]),
            one_of
        );
        assert_eq!(
            without_descriptions(&chat["$defs"]["ToolResultBlock"]["properties"]["content"]),
            one_of
        );
        for name in [
            "TextBlock",
            "ImageBlock",
            "Base64ImageSource",
            "ReferenceImageSource",
            "ImageMediaType",
        ] {
            assert!(!tool["$defs"][name].is_null(), "{name} is in TOOL");
            assert_eq!(
                without_descriptions(&tool["$defs"][name]),
                without_descriptions(&chat["$defs"][name]),
                "{name}"
            );
        }
    }

    #[test]
    fn a_truncated_result_of_blocks_ends_in_the_marker() {
        use crate::content::{Image, ToolContent, UserContent};
        use tool::{TRUNCATED_MARKER, render, render_content};
        let png = Image::new(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();
        let blocks = || {
            ToolContent::Blocks(vec![
                UserContent::Text("a".into()),
                UserContent::Image(png.clone()),
            ])
        };
        assert_eq!(
            render(blocks(), true),
            ToolContent::Blocks(vec![
                UserContent::Text("a".into()),
                UserContent::Image(png.clone()),
                UserContent::Text(TRUNCATED_MARKER.into()),
            ])
        );
        assert_eq!(render(blocks(), false), blocks());
        assert_eq!(
            render(ToolContent::Text("cut".into()), true),
            ToolContent::Text(render_content("cut", true))
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
