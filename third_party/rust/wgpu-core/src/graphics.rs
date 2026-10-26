//! A renderer-owned provider for graphics recording.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::fmt;

/// Binds graphics recording to the embedding renderer's queue and context.
///
/// The provider must not retain a core Device or Queue; it is retained by the
/// device and can otherwise form an ownership cycle.
pub trait GraphicsRecordingProvider: fmt::Debug + wgt::WasmNotSendSync {
    fn submission(self: Arc<Self>) -> Option<Arc<dyn GraphicsSubmissionProvider>> {
        None
    }

    /// # Safety
    /// The provider's graphics queue/context must be compatible with `device`.
    /// The caller must retain the logical device and referenced resources until
    /// every returned execution token has safely retired.
    unsafe fn create_encoder(
        &self,
        device: &dyn hal::DynDevice,
        native: Box<dyn hal::DynCommandEncoder>,
    ) -> Result<Box<dyn hal::DynCommandEncoder>, hal::DeviceError>;
}

pub use hal::shared::NativeRetirement;

pub struct GraphicsSubmitPart<'a> {
    pub lane: hal::shared::SharedQueueLane,
    pub queue: &'a dyn hal::DynQueue,
    pub buffer: &'a dyn hal::DynCommandBuffer,
}

/// Submission support supplied by the embedding renderer without retaining a
/// core Device or Queue. Submission and drain run under core guards and must not
/// reenter core. Recovery must invalidate all clients of a failed context.
pub trait GraphicsSubmissionProvider: fmt::Debug + wgt::WasmNotSendSync {
    fn seal_encoder(
        &self,
        encoder: &mut dyn hal::DynCommandEncoder,
    ) -> Result<(), hal::DeviceError>;

    fn submission_parts<'a>(
        &self,
        encoder: &'a dyn hal::DynCommandEncoder,
        buffers: &'a [Box<dyn hal::DynCommandBuffer>],
        queue: &'a dyn hal::DynQueue,
    ) -> Result<Vec<GraphicsSubmitPart<'a>>, hal::DeviceError>;

    /// Borrow the stable graphics endpoint from the same retained owner used
    /// by the recording provider; another wrapper of its native queue is invalid.
    fn graphics_queue(&self) -> Option<&dyn hal::DynQueue> {
        None
    }

    /// # Safety
    ///
    /// The requirements of SharedDeviceContext::submit_ordered apply.
    unsafe fn submit(
        &self,
        logical_queue: &dyn hal::DynQueue,
        segments: &[hal::shared::SharedSubmitSegment<'_>],
        terminal: (&dyn hal::DynFence, hal::FenceValue),
    ) -> Result<Box<dyn hal::shared::SharedSubmissionReceipt>, hal::shared::SharedSubmitError>;

    /// Ok or native Lost must establish retirement of every accepted operation.
    /// Logical device loss or admission closure must not be reported as native Lost.
    fn drain(&self) -> Result<(), hal::DeviceError>;
    /// Confirmed retirement permits shared-context recreation. Unconfirmed work
    /// requires eventual isolated-process teardown/restart so pending browser
    /// notifications settle without running native cleanup prematurely.
    fn request_recovery(&self, retirement: NativeRetirement);

    /// Must terminate the isolated graphics process without returning, unwinding
    /// or reentering core. It may run while destruction holds snatch protection.
    fn fatal_process_termination(&self) -> unsafe extern "C" fn() -> !;
}
