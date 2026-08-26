//! Booting a Gwead kernel configured as the Gwennol host.

use std::path::PathBuf;
use std::sync::Arc;

use gwead::kernel::native_impls::NativeStepImplTable;
use gwead::kernel::{Kernel, KernelConfig, KernelError};

use crate::host::{HostConfig, ProcessEnv, install};
use crate::operator::Operator;
use crate::secrets::OperatorSecrets;

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

/// Install the operator as this process's host and boot a kernel, with the
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
    })
}

/// [`boot`], with the host policy the frontend chose — today the
/// environment spawned processes get.
pub fn boot_with(host: HostConfig) -> Result<Kernel, BootError> {
    let operator = host.operator.clone();
    install(host).map_err(|_| BootError::AlreadyInstalled)?;
    let config = KernelConfig::default()
        .with_native_step_impls(NativeStepImplTable::discover()?)
        .with_secret_resolver(Arc::new(OperatorSecrets(operator)));
    Ok(Kernel::boot(config)?)
}
