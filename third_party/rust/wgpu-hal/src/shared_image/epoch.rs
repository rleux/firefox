use super::{ImageLease, ImageLeaseError};
use alloc::{boxed::Box, sync::Arc};
use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageCloseStatus {
    Claimed,
    Busy,
    AlreadyClosed,
}

pub struct ImageEpoch {
    #[cfg(vulkan)]
    pub(crate) inner: crate::vulkan::shared_image::epoch::Epoch,
    #[cfg(not(vulkan))]
    _private: (),
}

pub struct ImagePayload {
    #[cfg(vulkan)]
    pub(crate) _inner: crate::vulkan::shared_image::epoch::Payload,
    #[cfg(not(vulkan))]
    _private: (),
}

pub struct FinalizeReservation {
    #[cfg(vulkan)]
    pub(crate) inner: Option<crate::vulkan::shared_image::epoch::Finalizing>,
    #[cfg(not(vulkan))]
    _private: (),
}

pub struct ReturnedImage {
    #[cfg(vulkan)]
    pub(crate) inner: Option<crate::vulkan::shared_image::epoch::Writing>,
    #[cfg(not(vulkan))]
    _private: (),
}

macro_rules! opaque_debug {
    ($name:ident) => {
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(stringify!($name))
            }
        }
    };
}
opaque_debug!(ImageEpoch);
opaque_debug!(ImagePayload);
opaque_debug!(FinalizeReservation);
opaque_debug!(ReturnedImage);

impl ImageEpoch {
    pub fn descriptor(&self) -> &crate::TextureDescriptor<'static> {
        #[cfg(vulkan)]
        {
            return &self.inner.record.descriptor;
        }
        #[cfg(not(vulkan))]
        unreachable!("No native image epoch backend")
    }

    pub fn lease(&self) -> &Arc<dyn ImageLease> {
        #[cfg(vulkan)]
        {
            return &self.inner.lease;
        }
        #[cfg(not(vulkan))]
        unreachable!("No native image epoch backend")
    }

    pub fn ensure_open(&self) -> Result<(), ImageLeaseError> {
        #[cfg(vulkan)]
        {
            return self.inner.ensure_open();
        }
        #[cfg(not(vulkan))]
        Err(ImageLeaseError::UnsupportedBackend)
    }

    pub fn acquire_payload(self: &Arc<Self>) -> Result<ImagePayload, ImageLeaseError> {
        #[cfg(vulkan)]
        {
            return crate::vulkan::shared_image::epoch::payload(self.clone());
        }
        #[cfg(not(vulkan))]
        Err(ImageLeaseError::UnsupportedBackend)
    }

    pub fn validate_owner(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
    ) -> Result<(), ImageLeaseError> {
        #[cfg(vulkan)]
        {
            return self.inner.validate_owner(device, queue);
        }
        #[cfg(not(vulkan))]
        {
            let _ = (device, queue);
            Err(ImageLeaseError::UnsupportedBackend)
        }
    }

    pub fn begin_close(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
    ) -> Result<bool, ImageLeaseError> {
        #[cfg(vulkan)]
        {
            return self.inner.begin_close(device, queue);
        }
        #[cfg(not(vulkan))]
        {
            let _ = (device, queue);
            Err(ImageLeaseError::UnsupportedBackend)
        }
    }

    pub fn try_begin_close_if_unused(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
    ) -> Result<ImageCloseStatus, ImageLeaseError> {
        #[cfg(vulkan)]
        {
            return self.inner.try_begin_close_if_unused(device, queue);
        }
        #[cfg(not(vulkan))]
        {
            let _ = (device, queue);
            Err(ImageLeaseError::UnsupportedBackend)
        }
    }

    /// # Safety
    /// The sole close marker must have been accepted on this epoch's graphics queue.
    pub unsafe fn marker_accepted(&self) {
        #[cfg(vulkan)]
        self.inner.marker_accepted();
    }

    /// # Safety
    /// The accepted marker's complete native receipt must have retired successfully.
    pub unsafe fn marker_retired(&self) {
        #[cfg(vulkan)]
        self.inner.marker_retired();
    }

    pub fn abandon(&self) {
        #[cfg(vulkan)]
        self.inner.abandon();
    }

    pub fn try_take_returned(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
        graphics: &dyn crate::DynQueue,
    ) -> Result<Option<ReturnedImage>, ImageLeaseError> {
        #[cfg(vulkan)]
        {
            return self.inner.take_returned(device, queue, graphics);
        }
        #[cfg(not(vulkan))]
        {
            let _ = (device, queue, graphics);
            Err(ImageLeaseError::UnsupportedBackend)
        }
    }
}

impl ImagePayload {
    pub fn epoch(&self) -> &Arc<ImageEpoch> {
        #[cfg(vulkan)]
        {
            return &self._inner.epoch;
        }
        #[cfg(not(vulkan))]
        unreachable!("No native image epoch backend")
    }
}

impl ImageEpoch {
    pub fn is_closed(&self) -> bool {
        #[cfg(vulkan)]
        {
            return self.inner.is_closed();
        }
        #[cfg(not(vulkan))]
        true
    }
}

impl ReturnedImage {
    pub fn lease(&self) -> &Arc<dyn ImageLease> {
        #[cfg(vulkan)]
        {
            return &self.inner.as_ref().unwrap().lease;
        }
        #[cfg(not(vulkan))]
        unreachable!("No native returned image backend")
    }
}

impl FinalizeReservation {
    /// # Safety
    /// The producer tail was accepted, all core guards were released, and the
    /// matching logical texture expired before this publication.
    pub unsafe fn publish(self) -> Arc<ImageEpoch> {
        #[cfg(vulkan)]
        {
            let mut reservation = self;
            return reservation.inner.take().unwrap().publish();
        }
        #[cfg(not(vulkan))]
        unreachable!("No native image epoch backend")
    }
}

/// # Safety
/// Serialize producer access through finalization and expiry. Dropping a reserved
/// operation abandons this allocation for reuse, even before native submission.
pub unsafe fn reserve_finalization(
    lease: Arc<dyn ImageLease>,
    device: &dyn crate::DynDevice,
    queue: &dyn crate::DynQueue,
    texture: &dyn crate::DynTexture,
    graphics: &dyn crate::DynQueue,
    descriptor: &crate::TextureDescriptor<'_>,
) -> Result<FinalizeReservation, ImageLeaseError> {
    #[cfg(vulkan)]
    {
        return crate::vulkan::shared_image::epoch::reserve(
            lease, device, queue, texture, graphics, descriptor,
        );
    }
    #[cfg(not(vulkan))]
    {
        let _ = (lease, device, queue, texture, graphics, descriptor);
        Err(ImageLeaseError::UnsupportedBackend)
    }
}

/// # Safety
/// The descriptor is validated for the original live logical producer. The
/// returned alias must remain alive through all derived views and native work.
pub unsafe fn create_alias_from_returned(
    returned: ReturnedImage,
    device: &dyn crate::DynDevice,
    queue: &dyn crate::DynQueue,
    descriptor: &crate::TextureDescriptor<'_>,
) -> Result<Box<dyn crate::DynTexture>, ImageLeaseError> {
    #[cfg(vulkan)]
    {
        let mut returned = returned;
        return crate::vulkan::shared_image::epoch::reuse(
            returned.inner.take().unwrap(),
            device,
            queue,
            descriptor,
        );
    }
    #[cfg(not(vulkan))]
    {
        let _ = (returned, device, queue, descriptor);
        Err(ImageLeaseError::UnsupportedBackend)
    }
}
