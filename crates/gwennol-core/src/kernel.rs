//! Booting a Gwead kernel configured as the Gwennol host.

use std::path::PathBuf;
use std::sync::Arc;

use gwead::kernel::native_impls::NativeStepImplTable;
use gwead::kernel::{Kernel, KernelConfig, KernelError, RuntimeLimits};

use crate::host::{DEFAULT_ACTION_TIMEOUT, HostConfig, ProcessEnv, install};
use crate::operator::Operator;
use crate::secrets::OperatorSecrets;
use crate::spi;
use crate::steps;

/// The `host_fs` plugin manifest, as shipped.
pub const HOST_FS_MANIFEST: &str = include_str!("../resources/host_fs.json");
/// The `host_process` plugin manifest, as shipped.
pub const HOST_PROCESS_MANIFEST: &str = include_str!("../resources/host_process.json");
/// The `host_http` plugin manifest, as shipped.
pub const HOST_HTTP_MANIFEST: &str = include_str!("../resources/host_http.json");

/// Every host plugin manifest, in registration order.
pub const HOST_MANIFESTS: [&str; 3] = [HOST_FS_MANIFEST, HOST_PROCESS_MANIFEST, HOST_HTTP_MANIFEST];

gwead::native_step_impl!("gwennol.host_fs.read", steps::fs::fs_read);
gwead::native_step_impl!("gwennol.host_fs.write", steps::fs::fs_write);
gwead::native_step_impl!("gwennol.host_fs.list", steps::fs::fs_list);
gwead::native_step_impl!("gwennol.host_process.run", steps::process::process_run);
gwead::native_step_impl!("gwennol.host_http.get", steps::http::http_get);
gwead::native_step_impl!("gwennol.host_http.post", steps::http::http_post);

/// Why [`boot`] failed.
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    /// A host was already installed in this process.
    #[error("gwennol host already installed in this process")]
    AlreadyInstalled,
    /// Two crates submitted the same native implRef.
    #[error("native step implementation collision: {0}")]
    NativeImpls(#[from] gwead::kernel::native_impls::NativeImplCollision),
    /// The kernel refused to boot or to load a host manifest.
    #[error(transparent)]
    Kernel(#[from] KernelError),
}

/// Install the operator as this process's host and boot a kernel with the
/// `host_fs`, `host_process` and `host_http` plugins registered, with the
/// default [`ProcessEnv`].
///
/// Returns the kernel un-wrapped so the caller can register the bundled
/// SPIs and plugins before calling [`Kernel::into_arc`] — every step body
/// that checks a capability needs the `Arc`, so do not execute actions on
/// the bare kernel.
pub fn boot(operator: Arc<dyn Operator>, workspace_root: PathBuf) -> Result<Kernel, BootError> {
    boot_with(HostConfig {
        operator,
        workspace_root,
        process_env: ProcessEnv::default(),
        trusted_step_type_providers: Vec::new(),
        action_timeout: DEFAULT_ACTION_TIMEOUT,
    })
}

/// [`boot`], with the host policy the frontend chose — the environment
/// spawned processes get, and which plugins may supply script runtimes.
pub fn boot_with(host: HostConfig) -> Result<Kernel, BootError> {
    let operator = host.operator.clone();
    let mut config = KernelConfig::default()
        .with_native_step_impls(NativeStepImplTable::discover()?)
        .with_secret_resolver(Arc::new(OperatorSecrets(operator)))
        // The one kernel limit the host re-sizes: see
        // HostConfig::action_timeout for why gwead's default does not
        // fit an agent's actions.
        .with_limits(RuntimeLimits::default().with_default_wallclock_timeout(host.action_timeout));
    // The embedder half of the script-runtime authorization; the other
    // half is the provide:step_type: declaration in the trusted
    // plugin's own manifest. See HostConfig::trusted_step_type_providers.
    for provider in &host.trusted_step_type_providers {
        config = config.trusting_step_type_provider(provider.clone());
    }
    let mut kernel = Kernel::boot(config)?;
    // Contracts first: Gwead checks a plugin against a role's contract only
    // if the contract is already registered, so registering the SPI
    // definitions here — before any caller can add a plugin — is what makes
    // the check unskippable for the bundled roles.
    spi::register(&mut kernel)?;
    for manifest in HOST_MANIFESTS {
        kernel.register_plugin_from_json(manifest)?;
    }
    // The process-global host is installed only once everything fallible
    // has succeeded, so a failed boot leaves the process able to try
    // again. No step can run before this: executing actions needs the
    // kernel this function is about to return.
    install(host).map_err(|_| BootError::AlreadyInstalled)?;
    Ok(kernel)
}
