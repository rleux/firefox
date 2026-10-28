//! Allocation ownership and checked aliases for images on a shared native device.

use crate::{DynDevice, DynOpenDevice, DynTexture, TextureDescriptor};
use alloc::{boxed::Box, sync::Arc};
use core::{
    any::Any,
    fmt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// Charges every live physical shared image until confirmed allocator cleanup.
#[derive(Debug)]
pub struct AllocationBudget {
    capacity: usize,
    live: AtomicUsize,
    poisoned: AtomicBool,
}

impl AllocationBudget {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity != 0);
        Self {
            capacity,
            live: AtomicUsize::new(0),
            poisoned: AtomicBool::new(false),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
    pub fn live_allocations(&self) -> usize {
        self.live.load(Ordering::Acquire)
    }
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    pub(super) fn reserve(self: &Arc<Self>) -> Result<AllocationCredit, ImageLeaseError> {
        if self.is_poisoned() {
            return Err(crate::DeviceError::OutOfMemory.into());
        }
        self.live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < self.capacity).then(|| live + 1)
            })
            .map_err(|_| ImageLeaseError::Device(crate::DeviceError::OutOfMemory))?;
        let credit = AllocationCredit {
            budget: self.clone(),
            allocated: false,
            cleaned: false,
        };
        if self.is_poisoned() {
            return Err(crate::DeviceError::OutOfMemory.into());
        }
        Ok(credit)
    }
}

pub(super) struct AllocationCredit {
    budget: Arc<AllocationBudget>,
    allocated: bool,
    cleaned: bool,
}

impl AllocationCredit {
    pub(super) fn allocated(&mut self) {
        self.allocated = true;
    }
    pub(super) fn cleaned(&mut self) {
        self.cleaned = true;
    }
}

impl Drop for AllocationCredit {
    fn drop(&mut self) {
        if !self.allocated || self.cleaned {
            let previous = self.budget.live.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous != 0);
        } else {
            self.budget.poisoned.store(true, Ordering::Release);
        }
    }
}

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

/// Allocate one charged physical canvas image; aliases retain its original credit.
///
/// # Safety
/// The descriptor and graphics ownership requirements of allocate apply.
pub unsafe fn allocate_budgeted(
    graphics: Arc<DynOpenDevice>,
    descriptor: &TextureDescriptor<'_>,
    budget: Arc<AllocationBudget>,
) -> Result<Arc<dyn ImageLease>, ImageLeaseError> {
    #[cfg(vulkan)]
    if graphics.device.as_any().is::<crate::vulkan::Device>() {
        return unsafe {
            crate::vulkan::shared_image::allocate_budgeted(graphics, descriptor, budget)
        };
    }
    let _ = (graphics, descriptor, budget);
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

#[cfg(test)]
#[path = "shared_image/budget_tests.rs"]
mod budget_tests;
