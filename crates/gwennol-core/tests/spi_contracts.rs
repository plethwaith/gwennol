//! End-to-end: the bundled SPI contracts are registered at boot, plugins
//! are checked against them, and fixture implementations of each role are
//! dispatched *by role* through a real Gwead kernel. The tool-call wire
//! shapes exercised here are the ones `docs/SPI.md` documents.
//!
//! The host is a process singleton, so this binary boots one kernel with
//! every fixture plugin registered up front and shares it across tests.

use std::sync::{Arc, OnceLock};

use gwead::kernel::Kernel;
use gwead::serde_json::json;
use gwennol_core::{ApprovalRequest, Decision, Event, Operator, Turn, spi};

/// Allows everything, knows no secrets: contract dispatch needs no policy.
struct Permissive;

#[async_trait::async_trait]
impl Operator for Permissive {
    async fn approve(&self, _: ApprovalRequest) -> Decision {
        Decision::Allow
    }
    async fn secret(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn emit(&self, _: Event) {}
    async fn input(&self) -> Option<Turn> {
        None
    }
}

struct Fixture {
    kernel: Arc<Kernel>,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let workspace = tempfile::tempdir().unwrap().keep();
        let kernel = gwennol_core::boot(Arc::new(Permissive), workspace).unwrap();
        Fixture {
            kernel: kernel.into_arc(),
        }
    })
}

// ------------------------------------------------------- registration

#[test]
fn bundled_contracts_are_spi_definitions() {
    for (_, definition) in spi::SPI_DEFINITIONS {
        assert!(matches!(
            Kernel::manifest_kind(definition).unwrap(),
            gwead::kernel::ManifestKind::SpiDef
        ));
    }
}

#[tokio::test]
async fn boot_registers_both_roles() {
    let f = fixture();
    let roles = f.kernel.spi_registry().roles();
    assert!(roles.contains(&spi::llm_chat::ROLE), "{roles:?}");
    assert!(roles.contains(&spi::tool::ROLE), "{roles:?}");
}

#[tokio::test]
async fn a_provider_missing_the_chat_action_is_rejected() {
    // The reason boot registers the contracts first: with the definition
    // present, an incomplete claim is an error rather than a warning.
    // (On a bare Gwead kernel — registration is `&mut`, and the shared
    // fixture kernel is already behind its `Arc`.)
    let mut kernel = Kernel::boot(gwead::kernel::KernelConfig::default()).unwrap();
    for (role, definition) in spi::SPI_DEFINITIONS {
        kernel.register_spi_from_json(role, definition).unwrap();
    }
    let claim = json!({
        "name": "hollow_provider", "version": "0.0.0",
        "description": "claims LLM_CHAT but provides no chat action",
        "roles": [spi::llm_chat::ROLE],
        "actions": {"other": {"steps": [
            {"id": "s", "type": "let", "params": {"value": 1}}
        ]}}
    });
    let err = kernel
        .load_manifest(&claim.to_string())
        .register()
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("chat"), "{msg}");
    assert!(msg.contains(spi::llm_chat::ROLE), "{msg}");
}
