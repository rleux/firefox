use crate::{
    shared_image::{ImageLease, ImageLeaseError},
    Device as _,
};
use alloc::{boxed::Box, sync::Arc};
use core::{any::Any, fmt, sync::atomic::Ordering};

struct Allocation {
    owner: Arc<crate::DynOpenDevice>,
    texture: Option<super::Texture>,
    descriptor: crate::TextureDescriptor<'static>,
}

impl fmt::Debug for Allocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedImageAllocation")
            .field("descriptor", &self.descriptor)
            .finish_non_exhaustive()
    }
}

impl ImageLease for Allocation {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        if let Some(texture) = self.texture.take() {
            let owner = self
                .owner
                .device
                .as_any()
                .downcast_ref::<super::Device>()
                .unwrap();
            unsafe { owner.destroy_texture(texture) };
        }
    }
}

fn compatible(
    allocation: &crate::TextureDescriptor<'_>,
    alias: &crate::TextureDescriptor<'_>,
) -> bool {
    allocation.size == alias.size
        && allocation.mip_level_count == alias.mip_level_count
        && allocation.sample_count == alias.sample_count
        && allocation.dimension == alias.dimension
        && allocation.format == alias.format
        && allocation.memory_flags == alias.memory_flags
        && allocation.usage.contains(alias.usage)
        && alias
            .view_formats
            .iter()
            .all(|format| *format == allocation.format || allocation.view_formats.contains(format))
}

fn validate_features(
    device: &super::Device,
    desc: &crate::TextureDescriptor<'_>,
) -> Result<(), ImageLeaseError> {
    let storage = wgt::TextureUses::STORAGE_READ_ONLY
        | wgt::TextureUses::STORAGE_WRITE_ONLY
        | wgt::TextureUses::STORAGE_READ_WRITE
        | wgt::TextureUses::STORAGE_ATOMIC;
    let mut required = desc.format.required_features();
    for format in &desc.view_formats {
        required |= format.required_features();
        if format.remove_srgb_suffix() != desc.format.remove_srgb_suffix() {
            return Err(ImageLeaseError::InvalidDescriptor);
        }
    }
    if desc.format == wgt::TextureFormat::Bgra8Unorm && desc.usage.intersects(storage) {
        required |= wgt::Features::BGRA8UNORM_STORAGE;
    }
    if !device.shared.features.contains(required) {
        return Err(ImageLeaseError::InvalidDescriptor);
    }
    Ok(())
}

pub(crate) unsafe fn allocate(
    graphics: Arc<crate::DynOpenDevice>,
    desc: &crate::TextureDescriptor<'_>,
) -> Result<Arc<dyn ImageLease>, ImageLeaseError> {
    let device = graphics
        .device
        .as_any()
        .downcast_ref::<super::Device>()
        .ok_or(ImageLeaseError::InvalidEndpoint)?;
    let queue = graphics
        .queue
        .as_any()
        .downcast_ref::<super::Queue>()
        .ok_or(ImageLeaseError::InvalidEndpoint)?;
    let group = device
        .shared
        .shared_queue_group
        .as_ref()
        .ok_or(ImageLeaseError::InvalidEndpoint)?;
    if device.shared.queue_index != 0
        || !Arc::ptr_eq(&device.shared, &queue.device)
        || device.shared.raw_queue != queue.raw
    {
        return Err(ImageLeaseError::InvalidEndpoint);
    }
    validate_features(device, desc)?;
    let texture = {
        let _host = group.host[0].lock();
        if group.closed.load(Ordering::Acquire) {
            return Err(ImageLeaseError::Closed);
        }
        unsafe { device.create_texture(desc) }?
    };
    let descriptor = crate::TextureDescriptor {
        label: None,
        size: desc.size,
        mip_level_count: desc.mip_level_count,
        sample_count: desc.sample_count,
        dimension: desc.dimension,
        format: desc.format,
        usage: desc.usage,
        memory_flags: desc.memory_flags,
        view_formats: desc.view_formats.clone(),
    };
    Ok(Arc::new(Allocation {
        owner: graphics,
        texture: Some(texture),
        descriptor,
    }))
}

fn alias_device(
    allocation: &Allocation,
    target: &super::Device,
) -> Result<Arc<super::shared::QueueGroup>, ImageLeaseError> {
    let owner = allocation
        .owner
        .device
        .as_any()
        .downcast_ref::<super::Device>()
        .unwrap();
    let group = owner.shared.shared_queue_group.as_ref().unwrap().clone();
    let target_group = target
        .shared
        .shared_queue_group
        .as_ref()
        .ok_or(ImageLeaseError::IncompatibleDevice)?;
    if !Arc::ptr_eq(&group, target_group)
        || !Arc::ptr_eq(&owner.shared.instance, &target.shared.instance)
        || owner.shared.raw.handle() != target.shared.raw.handle()
        || owner.shared.physical_device != target.shared.physical_device
        || target.shared.queue_index > 1
        || !owner.shared.features.contains(target.shared.features)
    {
        return Err(ImageLeaseError::IncompatibleDevice);
    }
    Ok(group)
}

pub(crate) unsafe fn create_alias(
    lease: Arc<dyn ImageLease>,
    target: &super::Device,
    desc: &crate::TextureDescriptor<'_>,
) -> Result<Box<dyn crate::DynTexture>, ImageLeaseError> {
    let allocation = lease
        .as_any()
        .downcast_ref::<Allocation>()
        .ok_or(ImageLeaseError::IncompatibleDevice)?;
    let group = alias_device(allocation, target)?;
    if !compatible(&allocation.descriptor, desc) {
        return Err(ImageLeaseError::InvalidDescriptor);
    }
    validate_features(target, desc)?;
    let raw = allocation.texture.as_ref().unwrap().raw;
    let _host = group.host[target.shared.queue_index as usize].lock();
    if group.closed.load(Ordering::Acquire) {
        return Err(ImageLeaseError::Closed);
    }
    let mut alias_descriptor = desc.clone();
    alias_descriptor.label = None;
    let alias = unsafe {
        target.texture_from_raw(
            raw,
            &alias_descriptor,
            Some(Box::new(move || drop(lease))),
            super::TextureMemory::External,
        )
    };
    unsafe { target.add_raw_texture(&alias) };
    Ok(Box::new(alias))
}

pub(crate) fn validate_alias(
    lease: &dyn ImageLease,
    target: &super::Device,
    texture: &super::Texture,
    graphics_queue: &dyn crate::DynQueue,
    descriptor: &crate::TextureDescriptor<'_>,
) -> Result<(), ImageLeaseError> {
    let allocation = lease
        .as_any()
        .downcast_ref::<Allocation>()
        .ok_or(ImageLeaseError::IncompatibleDevice)?;
    let group = alias_device(allocation, target)?;
    let original = allocation.texture.as_ref().unwrap();
    if !compatible(&allocation.descriptor, descriptor)
        || !core::ptr::eq(allocation.owner.queue.as_ref(), graphics_queue)
        || !allocation
            .descriptor
            .usage
            .contains(wgt::TextureUses::RESOURCE)
        || texture.raw != original.raw
        || texture.format != original.format
        || texture.copy_size.width != original.copy_size.width
        || texture.copy_size.height != original.copy_size.height
        || texture.copy_size.depth != original.copy_size.depth
        || !matches!(&texture.memory, super::TextureMemory::External)
        || texture.drop_guard.is_none()
    {
        return Err(ImageLeaseError::InvalidDescriptor);
    }
    let _host = group.host[target.shared.queue_index as usize].lock();
    if group.closed.load(Ordering::Acquire) {
        return Err(ImageLeaseError::Closed);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
