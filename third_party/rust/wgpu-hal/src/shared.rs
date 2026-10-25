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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeRetirement {
    Confirmed,
    Unconfirmed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedQueueLane {
    Graphics,
    Compute,
}

pub struct SharedSubmitSegment<'a> {
    pub lane: SharedQueueLane,
    pub queue: &'a dyn crate::DynQueue,
    pub command_buffers: &'a [&'a dyn crate::DynCommandBuffer],
    pub synchronization: Option<&'a crate::external_sync::SubmissionSync>,
}

impl fmt::Debug for SharedSubmitSegment<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedSubmitSegment")
            .field("lane", &self.lane)
            .field("command_buffer_count", &self.command_buffers.len())
            .finish_non_exhaustive()
    }
}

/// Owns native synchronization until the submitted work has retired.
/// Partial-error receipts cannot be waited on; use a real context drain.
/// Dropping a receipt requires confirmed retirement of all accepted work.
pub trait SharedSubmissionReceipt: fmt::Debug + wgt::WasmNotSendSync {
    fn poll(&self) -> Result<bool, DeviceError>;
    fn wait(&self, timeout: Option<core::time::Duration>) -> Result<bool, DeviceError>;
}

#[derive(Debug, thiserror::Error)]
#[error("Shared submission failed: {error}")]
pub struct SharedSubmitError {
    pub error: DeviceError,
    pub receipt: Option<alloc::boxed::Box<dyn SharedSubmissionReceipt>>,
}

pub trait SharedDeviceContext: fmt::Debug + wgt::WasmNotSendSync {
    fn identity(&self) -> SharedDeviceIdentity;

    /// Clone a lease on the original native instance, restricted to the selected
    /// physical adapter, without reconfiguring the instance.
    fn clone_instance(&self) -> (wgt::Backend, alloc::boxed::Box<dyn crate::DynInstance>);

    /// Open a private logical device on the reserved compute queue.
    ///
    /// This does not create a native device or enable additional native features.
    /// Queue idle waits normally cover this logical endpoint's accepted work.
    /// Closed or uncertain contexts require a real shared native drain instead.
    ///
    /// # Safety
    ///
    /// Returned resource and submission owners must remain alive until native
    /// retirement is confirmed. Adapter membership and feature/limit subsets are
    /// checked before backend calls.
    unsafe fn open_compute_device(
        self: Arc<Self>,
        adapter: &dyn DynAdapter,
        features: wgt::Features,
        limits: &wgt::Limits,
        memory_hints: &wgt::MemoryHints,
    ) -> Result<DynOpenDevice, SharedDeviceError>;

    /// Enqueue an ordered sequence and signal the caller's fence only at its end.
    ///
    /// # Safety
    ///
    /// Queues and buffers must be valid and live; their backend, native context
    /// and queue-family compatibility are checked before enqueue. The fence must
    /// belong to this context, and recorded command states must be truthful.
    /// Retain all encoders, resources, queues, fence and receipt until confirmed native retirement.
    /// An error carrying a receipt may have accepted work and requires real drain.
    unsafe fn submit_ordered(
        &self,
        logical_compute_queue: &dyn crate::DynQueue,
        segments: &[SharedSubmitSegment<'_>],
        terminal: (&dyn crate::DynFence, crate::FenceValue),
    ) -> Result<alloc::boxed::Box<dyn SharedSubmissionReceipt>, SharedSubmitError>;

    /// Submit ordinary renderer work with dependencies bound to this graphics call.
    ///
    /// # Safety
    /// Queue::submit lifetime requirements apply. Synchronization must obey its
    /// producer/consumer rules. Preserve every owner and receipt through retirement.
    unsafe fn submit_graphics(
        &self,
        queue: &dyn crate::DynQueue,
        buffers: &[&dyn crate::DynCommandBuffer],
        surfaces: &[&dyn crate::DynSurfaceTexture],
        terminal: (&dyn crate::DynFence, crate::FenceValue),
        synchronization: &crate::external_sync::SubmissionSync,
    ) -> Result<alloc::boxed::Box<dyn SharedSubmissionReceipt>, SharedSubmitError>;

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
    #[error("Shared devices require native timeline semaphores")]
    TimelineSemaphoresUnavailable,
    #[error("The adapter is not the shared context's original instance and physical device")]
    IncompatibleAdapter,
    #[error("Native features were not enabled when the shared context was created: {0:?}")]
    UnsupportedFeatures(wgt::Features),
    #[error("Requested limits exceed the shared context's native limits")]
    LimitsExceeded,
    #[error("The shared context no longer accepts new work")]
    Closed,
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
