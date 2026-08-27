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

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded copies must be byte-identical to the canonical
    /// documents in `plugins/spi/`. Skips when the repository layout is
    /// absent (a packaged crate has only the copies).
    #[test]
    fn embedded_contracts_match_the_canonical_documents() {
        for (role, embedded, canonical) in [
            (llm_chat::ROLE, llm_chat::DEFINITION, "llm_chat.json"),
            (tool::ROLE, tool::DEFINITION, "tool.json"),
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../plugins/spi")
                .join(canonical);
            let Ok(canonical) = std::fs::read_to_string(&path) else {
                eprintln!("skipping {role}: no {} here", path.display());
                continue;
            };
            assert_eq!(
                embedded, canonical,
                "{role}: crates/gwennol-core/resources/spi/ has drifted from plugins/spi/ — copy the canonical file over"
            );
        }
    }
}
