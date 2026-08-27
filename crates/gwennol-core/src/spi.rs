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

/// The `TOOL` role: a tool the model can call.
pub mod tool {
    /// The role name, as used in `roles` and `invoke:role:` permissions.
    pub const ROLE: &str = "TOOL";
    /// The one required action: model-supplied arguments in, one uniform
    /// `{content, is_error}` result out.
    pub const CALL: &str = "call";
    /// The contract document, as shipped. Canonical file:
    /// `plugins/spi/tool.json`; see [`super::llm_chat::DEFINITION`].
    pub const DEFINITION: &str = include_str!("../resources/spi/tool.json");
}

/// Every bundled contract as `(role, definition)`, in registration order.
pub const SPI_DEFINITIONS: [(&str, &str); 2] = [
    (llm_chat::ROLE, llm_chat::DEFINITION),
    (tool::ROLE, tool::DEFINITION),
];

/// Register every bundled contract with `kernel`.
///
/// Must run before any plugin claims a bundled role: Gwead checks a claim
/// against a contract only when the contract is already registered — an
/// unknown role loads with nothing but a warning. [`crate::boot`] calls
/// this; any other registration path (a test kernel, a future embedder
/// seam) must call it too rather than hand-rolling the loop.
pub fn register(kernel: &mut gwead::kernel::Kernel) -> Result<(), gwead::kernel::KernelError> {
    for (role, definition) in SPI_DEFINITIONS {
        kernel.register_spi_from_json(role, definition)?;
    }
    Ok(())
}

/// Why [`harvest_tools`] refused the kernel's tool inventory.
#[derive(Debug, thiserror::Error)]
pub enum HarvestError {
    /// Two plugins advertise the same `tool.name`, so "the descriptor
    /// with that name" is ambiguous — and real provider APIs reject
    /// duplicate tools anyway.
    #[error("duplicate tool name {0:?}: two TOOL plugins advertise it")]
    DuplicateToolName(String),
}

/// The tools the model may call, as harvested descriptors.
///
/// This is the one implementation of the harvest rules in `docs/SPI.md`
/// — Gwead's `get_tool_descriptors()` collects a `tool` block from *any*
/// action of *any* plugin, in unspecified order, so the raw list is not
/// what the model gets:
///
/// - only descriptors whose plugin fulfils the [`tool::ROLE`] role via
///   its [`tool::CALL`] action survive — anything else never faced the
///   contract check;
/// - duplicate tool names are refused, not resolved;
/// - the result is sorted by name, because tool order is model-visible
///   and prefix-sensitive: an unspecified order changes the prompt every
///   process start and busts provider-side prompt caching.
///
/// Each descriptor maps onto a `chat` `tools` entry as `tool_name →
/// name`, `description → description`, `parameters → input_schema`, and
/// its `plugin_key`/`action_name` say exactly what to execute when the
/// model names the tool.
pub fn harvest_tools(
    kernel: &gwead::kernel::Kernel,
) -> Result<Vec<gwead::kernel::registry::ToolDescriptor>, HarvestError> {
    let fulfillers = kernel.role_candidates(None, tool::ROLE);
    let mut descriptors: Vec<_> = kernel
        .registry()
        .get_tool_descriptors()
        .into_iter()
        .filter(|d| fulfillers.contains(&d.plugin_key) && d.action_name == tool::CALL)
        .collect();
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

    /// The embedded copies must be byte-identical to the canonical
    /// documents in `plugins/spi/`. The one legitimate skip is the whole
    /// directory being absent (a packaged crate has only the copies);
    /// inside a present repository layout, every read must succeed and
    /// match, so a moved or misnamed canonical file fails loudly instead
    /// of silently disarming the repo's only drift check.
    #[test]
    fn embedded_contracts_match_the_canonical_documents() {
        let canonical_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/spi");
        if !canonical_dir.is_dir() {
            eprintln!(
                "skipping: no repository layout at {}",
                canonical_dir.display()
            );
            return;
        }
        for (role, embedded, file) in [
            (llm_chat::ROLE, llm_chat::DEFINITION, "llm_chat.json"),
            (tool::ROLE, tool::DEFINITION, "tool.json"),
        ] {
            let path = canonical_dir.join(file);
            let canonical = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{role}: cannot read {}: {e}", path.display()));
            assert_eq!(
                embedded, canonical,
                "{role}: crates/gwennol-core/resources/spi/ has drifted from plugins/spi/ — copy the canonical file over"
            );
        }
    }
}
