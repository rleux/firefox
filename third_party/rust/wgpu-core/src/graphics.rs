//! A renderer-owned provider for graphics recording.

use alloc::boxed::Box;
use core::fmt;

/// Binds graphics recording to the embedding renderer's queue and context.
///
/// The provider must not retain a core Device or Queue; it is retained by the
/// device and can otherwise form an ownership cycle.
pub trait GraphicsRecordingProvider: fmt::Debug + wgt::WasmNotSendSync {
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
