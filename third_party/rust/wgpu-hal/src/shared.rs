//! Native device sharing without exposing backend handles.

use alloc::sync::Arc;
use core::fmt;

use crate::{DeviceError, DynAdapter, DynOpenDevice};

#[derive(Clone, Debug)]
pub struct SharedDeviceIdentity(Arc<()>);

impl SharedDeviceIdentity {
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }
}

impl PartialEq for SharedDeviceIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for SharedDeviceIdentity {}

pub trait SharedDeviceContext: fmt::Debug + wgt::WasmNotSendSync {
    fn identity(&self) -> SharedDeviceIdentity;

    /// Clone a lease on the original native instance without reconfiguring it.
    /// Adapter enumeration still needs to select this context's physical device.
    fn clone_instance(&self) -> (wgt::Backend, alloc::boxed::Box<dyn crate::DynInstance>);

    /// Permanently close submission admission and wait for accepted native work.
    ///
    /// An error is not proof of retirement: callers must retain all endpoint,
    /// encoder and resource owners until retirement can be established.
    fn drain(&self) -> Result<(), DeviceError>;
}

pub struct SharedDeviceEndpoints {
    pub context: Arc<dyn SharedDeviceContext>,
    pub graphics: DynOpenDevice,
    pub compute: DynOpenDevice,
}

impl fmt::Debug for SharedDeviceEndpoints {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedDeviceEndpoints")
            .field("context", &self.context)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum SharedDeviceError {
    #[error("The adapter backend does not support shared devices")]
    UnsupportedBackend,
    #[error("Shared devices require two graphics/compute queues in the presentation-compatible queue family")]
    IndependentQueuesUnavailable,
    #[error(transparent)]
    Device(#[from] DeviceError),
}

/// Open independent graphics and compute endpoints on one native device.
/// Surface configuration and presentation are not supported by these endpoints.
///
/// The Vulkan implementation currently requires two universal queues in family
/// zero, matching the existing adapter and presentation capability checks.
///
/// # Safety
///
/// Features and limits must satisfy DynAdapter::open's requirements. All
/// submitted work must retire before its endpoints and resources are destroyed,
/// including when submission or drain reports an error.
pub unsafe fn open_shared_device(
    adapter: &dyn DynAdapter,
    features: wgt::Features,
    limits: &wgt::Limits,
    memory_hints: &wgt::MemoryHints,
) -> Result<SharedDeviceEndpoints, SharedDeviceError> {
    #[cfg(vulkan)]
    if let Some(adapter) = adapter.as_any().downcast_ref::<crate::vulkan::Adapter>() {
        return unsafe {
            crate::vulkan::shared::open_shared_device(adapter, features, limits, memory_hints)
        };
    }
    let _ = (adapter, features, limits, memory_hints);
    Err(SharedDeviceError::UnsupportedBackend)
}
