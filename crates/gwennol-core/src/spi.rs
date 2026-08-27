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
    /// The contract document, as shipped (`plugins/spi/llm_chat.json`).
    // Embedded from outside the crate directory; bundling these into a
    // published crate is a distribution-milestone problem (see ROADMAP,
    // "Beyond the MVP").
    pub const DEFINITION: &str = include_str!("../../../plugins/spi/llm_chat.json");
}

/// The `TOOL` role: a tool the model can call.
pub mod tool {
    /// The role name, as used in `roles` and `invoke:role:` permissions.
    pub const ROLE: &str = "TOOL";
    /// The one required action: model-supplied arguments in, one uniform
    /// `{content, is_error}` result out.
    pub const CALL: &str = "call";
    /// The contract document, as shipped (`plugins/spi/tool.json`).
    pub const DEFINITION: &str = include_str!("../../../plugins/spi/tool.json");
}

/// Every bundled contract as `(role, definition)`, in registration order.
pub const SPI_DEFINITIONS: [(&str, &str); 2] = [
    (llm_chat::ROLE, llm_chat::DEFINITION),
    (tool::ROLE, tool::DEFINITION),
];
