//! Allocation ownership and checked aliases for images on a shared native device.

use crate::{DynDevice, DynOpenDevice, DynTexture, TextureDescriptor};
use alloc::{boxed::Box, sync::Arc};
use core::{any::Any, fmt};

pub trait ImageLease: fmt::Debug + wgt::WasmNotSendSync {
    fn as_any(&self) -> &dyn Any;
}

#[derive(Debug, thiserror::Error)]
pub enum ImageLeaseError {
    #[error("Shared images require the Vulkan backend")]
    UnsupportedBackend,
    #[error("Allocation requires a matching shared graphics device and queue")]
    InvalidEndpoint,
    #[error("The alias device belongs to another native context")]
    IncompatibleDevice,
    #[error("The descriptor is incompatible with the allocation or enabled features")]
    InvalidDescriptor,
    #[error("The shared native context or image epoch is closed")]
    Closed,
    #[error("The image epoch or reservation is not current")]
    InvalidEpoch,
    #[error("The image allocation was abandoned for reuse")]
    Abandoned,
    #[error("The logical producer endpoint no longer exists")]
    ProducerGone,
    #[error(transparent)]
    Device(#[from] crate::DeviceError),
}

/// Allocate physical storage through the retained graphics allocator.
///
/// # Safety
/// The descriptor must be validated for this adapter and its enabled features.
/// Keep a lease or alias alive through all derived views and accepted GPU work.
/// Allocation does not initialize contents or establish a texture layout.
pub unsafe fn allocate(
    graphics: Arc<DynOpenDevice>,
    descriptor: &TextureDescriptor<'_>,
) -> Result<Arc<dyn ImageLease>, ImageLeaseError> {
    #[cfg(vulkan)]
    if graphics.device.as_any().is::<crate::vulkan::Device>() {
        return unsafe { crate::vulkan::shared_image::allocate(graphics, descriptor) };
    }
    let _ = (graphics, descriptor);
    Err(ImageLeaseError::UnsupportedBackend)
}

/// Create a logical texture alias without transferring allocation ownership.
///
/// # Safety
/// The descriptor must be validated for the target logical device. Retain a
/// lease or alias through all derived views and accepted GPU work. Call the
/// target device's destroy_texture exactly once for each returned alias.
/// Aliases share storage; the caller coordinates initialization and layouts.
pub unsafe fn create_alias(
    lease: Arc<dyn ImageLease>,
    target: &dyn DynDevice,
    descriptor: &TextureDescriptor<'_>,
) -> Result<Box<dyn DynTexture>, ImageLeaseError> {
    #[cfg(vulkan)]
    if let Some(target) = target.as_any().downcast_ref::<crate::vulkan::Device>() {
        return unsafe { crate::vulkan::shared_image::create_alias(lease, target, descriptor) };
    }
    let _ = (lease, target, descriptor);
    Err(ImageLeaseError::UnsupportedBackend)
}

/// Validate a live alias and sampling capability against its exact graphics owner.
pub fn validate_alias(
    lease: &dyn ImageLease,
    target: &dyn DynDevice,
    texture: &dyn DynTexture,
    graphics_queue: &dyn crate::DynQueue,
    descriptor: &crate::TextureDescriptor<'_>,
) -> Result<(), ImageLeaseError> {
    #[cfg(vulkan)]
    if let (Some(target), Some(texture)) = (
        target.as_any().downcast_ref::<crate::vulkan::Device>(),
        texture.as_any().downcast_ref::<crate::vulkan::Texture>(),
    ) {
        return crate::vulkan::shared_image::validate_alias(
            lease,
            target,
            texture,
            graphics_queue,
            descriptor,
        );
    }
    let _ = (lease, target, texture, graphics_queue, descriptor);
    Err(ImageLeaseError::UnsupportedBackend)
}

mod epoch;
pub use epoch::{create_alias_from_returned, reserve_finalization};
pub use epoch::{FinalizeReservation, ImageEpoch, ImagePayload, ReturnedImage};
