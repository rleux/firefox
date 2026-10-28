use super::{Allocation, ImageLease, ImageLeaseError};
use crate::shared_image::{
    FinalizeReservation, ImageCloseStatus, ImageEpoch, ImagePayload, ReturnedImage,
};
use crate::vulkan::shared::QueueProgress;
use crate::{
    vulkan::{Device, DeviceShared, Queue, ResourceIdentity, Texture},
    Device as _,
};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use ash::vk;
use core::sync::atomic::Ordering;
use wgpu_sync::Mutex;

#[derive(Clone)]
struct Producer {
    device: Weak<DeviceShared>,
    progress: Weak<QueueProgress>,
}

impl Producer {
    fn capture(device: &Device, queue: &Queue) -> Result<Self, ImageLeaseError> {
        if device.shared.queue_index != 1 || !Arc::ptr_eq(&device.shared, &queue.device) {
            return Err(ImageLeaseError::InvalidEndpoint);
        }
        Ok(Self {
            device: Arc::downgrade(&device.shared),
            progress: Arc::downgrade(
                queue
                    .progress
                    .as_ref()
                    .ok_or(ImageLeaseError::InvalidEndpoint)?,
            ),
        })
    }

    fn validate(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
    ) -> Result<(Arc<DeviceShared>, Arc<QueueProgress>), ImageLeaseError> {
        let original = self.device.upgrade().ok_or(ImageLeaseError::ProducerGone)?;
        let progress = self
            .progress
            .upgrade()
            .ok_or(ImageLeaseError::ProducerGone)?;
        let device = device
            .as_any()
            .downcast_ref::<Device>()
            .ok_or(ImageLeaseError::IncompatibleDevice)?;
        let queue = queue
            .as_any()
            .downcast_ref::<Queue>()
            .ok_or(ImageLeaseError::IncompatibleDevice)?;
        if !Arc::ptr_eq(&original, &device.shared)
            || !Arc::ptr_eq(&original, &queue.device)
            || !queue
                .progress
                .as_ref()
                .is_some_and(|saved| Arc::ptr_eq(&progress, saved))
        {
            return Err(ImageLeaseError::IncompatibleDevice);
        }
        Ok((original, progress))
    }

    fn same(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.device, &other.device) && Weak::ptr_eq(&self.progress, &other.progress)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    None,
    Reserved,
    Accepted,
    Retired,
    Abandoned,
}

struct Status {
    closed: bool,
    payloads: usize,
    marker: Marker,
}

impl Status {
    fn begin_close(&mut self, only_unused: bool) -> ImageCloseStatus {
        if self.closed {
            return ImageCloseStatus::AlreadyClosed;
        }
        if only_unused && self.payloads != 0 {
            return ImageCloseStatus::Busy;
        }
        self.closed = true;
        self.marker = Marker::Reserved;
        ImageCloseStatus::Claimed
    }
}

fn ready(status: &Status) -> bool {
    status.closed && status.marker == Marker::Retired && status.payloads == 0
}

pub(crate) struct Record {
    pub(crate) descriptor: crate::TextureDescriptor<'static>,
    producer: Producer,
    status: Mutex<Status>,
}

enum Phase {
    Fresh,
    Writing {
        serial: u64,
        producer: Producer,
        alias: Option<ResourceIdentity<vk::Image>>,
        descriptor: Option<crate::TextureDescriptor<'static>>,
    },
    Finalizing(u64),
    Published {
        serial: u64,
        epoch: Weak<ImageEpoch>,
        _record: Arc<Record>,
    },
    Abandoned,
}

pub(super) struct Slot {
    phase: Phase,
}
impl Default for Slot {
    fn default() -> Self {
        Self {
            phase: Phase::Fresh,
        }
    }
}

fn allocation(lease: &dyn ImageLease) -> Result<&Allocation, ImageLeaseError> {
    lease
        .as_any()
        .downcast_ref::<Allocation>()
        .ok_or(ImageLeaseError::IncompatibleDevice)
}

fn live(lease: &dyn ImageLease) -> Result<&Allocation, ImageLeaseError> {
    let allocation = allocation(lease)?;
    let device = allocation
        .owner
        .device
        .as_any()
        .downcast_ref::<Device>()
        .unwrap();
    if device
        .shared
        .shared_queue_group
        .as_ref()
        .unwrap()
        .closed
        .load(Ordering::Acquire)
    {
        return Err(ImageLeaseError::Closed);
    }
    Ok(allocation)
}

fn owned(desc: &crate::TextureDescriptor<'_>) -> crate::TextureDescriptor<'static> {
    crate::TextureDescriptor {
        label: None,
        size: desc.size,
        mip_level_count: desc.mip_level_count,
        sample_count: desc.sample_count,
        dimension: desc.dimension,
        format: desc.format,
        usage: desc.usage,
        memory_flags: desc.memory_flags,
        view_formats: desc.view_formats.clone(),
    }
}

fn same_descriptor(a: &crate::TextureDescriptor<'_>, b: &crate::TextureDescriptor<'_>) -> bool {
    a.size == b.size
        && a.mip_level_count == b.mip_level_count
        && a.sample_count == b.sample_count
        && a.dimension == b.dimension
        && a.format == b.format
        && a.usage == b.usage
        && a.memory_flags == b.memory_flags
        && a.view_formats == b.view_formats
}

fn current(slot: &Slot, serial: u64) -> Result<(), ImageLeaseError> {
    match &slot.phase {
        Phase::Published {
            serial: saved,
            epoch,
            ..
        } if *saved == serial => {
            if epoch.strong_count() == 0 {
                Err(ImageLeaseError::Abandoned)
            } else {
                Ok(())
            }
        }
        Phase::Abandoned => Err(ImageLeaseError::Abandoned),
        _ => Err(ImageLeaseError::InvalidEpoch),
    }
}

pub(crate) struct Epoch {
    pub(crate) lease: Arc<dyn ImageLease>,
    pub(crate) record: Arc<Record>,
    serial: u64,
}

impl Epoch {
    pub(crate) fn is_closed(&self) -> bool {
        self.record.status.lock().closed
    }

    pub(crate) fn ensure_open(&self) -> Result<(), ImageLeaseError> {
        let allocation = live(self.lease.as_ref())?;
        let slot = allocation.publication.lock();
        current(&slot, self.serial)?;
        if self.record.status.lock().closed {
            Err(ImageLeaseError::Closed)
        } else {
            Ok(())
        }
    }

    pub(crate) fn validate_owner(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
    ) -> Result<(), ImageLeaseError> {
        let allocation = live(self.lease.as_ref())?;
        let original = allocation
            .owner
            .device
            .as_any()
            .downcast_ref::<Device>()
            .unwrap();
        let device = device
            .as_any()
            .downcast_ref::<Device>()
            .ok_or(ImageLeaseError::IncompatibleDevice)?;
        if !Arc::ptr_eq(&original.shared, &device.shared)
            || !core::ptr::eq(allocation.owner.queue.as_ref(), queue)
        {
            return Err(ImageLeaseError::InvalidEndpoint);
        }
        Ok(())
    }

    pub(crate) fn begin_close(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
    ) -> Result<bool, ImageLeaseError> {
        self.close(device, queue, false)
            .map(|status| status == ImageCloseStatus::Claimed)
    }

    pub(crate) fn try_begin_close_if_unused(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
    ) -> Result<ImageCloseStatus, ImageLeaseError> {
        self.close(device, queue, true)
    }

    fn close(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
        only_unused: bool,
    ) -> Result<ImageCloseStatus, ImageLeaseError> {
        self.validate_owner(device, queue)?;
        let allocation = allocation(self.lease.as_ref())?;
        let slot = allocation.publication.lock();
        current(&slot, self.serial)?;
        let mut status = self.record.status.lock();
        Ok(status.begin_close(only_unused))
    }

    pub(crate) fn marker_accepted(&self) {
        let allocation = allocation(self.lease.as_ref()).unwrap();
        let slot = allocation.publication.lock();
        if current(&slot, self.serial).is_err() {
            return;
        }
        let mut status = self.record.status.lock();
        if status.marker == Marker::Reserved {
            status.marker = Marker::Accepted;
        }
    }

    pub(crate) fn marker_retired(&self) {
        let allocation = allocation(self.lease.as_ref()).unwrap();
        let slot = allocation.publication.lock();
        if current(&slot, self.serial).is_err() {
            return;
        }
        let mut status = self.record.status.lock();
        if status.marker == Marker::Accepted {
            status.marker = Marker::Retired;
        }
    }

    pub(crate) fn abandon(&self) {
        let allocation = allocation(self.lease.as_ref()).unwrap();
        let mut slot = allocation.publication.lock();
        if current(&slot, self.serial).is_err() {
            return;
        }
        let mut status = self.record.status.lock();
        status.closed = true;
        status.marker = Marker::Abandoned;
        slot.phase = Phase::Abandoned;
    }

    pub(crate) fn take_returned(
        &self,
        device: &dyn crate::DynDevice,
        queue: &dyn crate::DynQueue,
        graphics: &dyn crate::DynQueue,
    ) -> Result<Option<ReturnedImage>, ImageLeaseError> {
        let allocation = live(self.lease.as_ref())?;
        if !core::ptr::eq(allocation.owner.queue.as_ref(), graphics) {
            return Err(ImageLeaseError::InvalidEndpoint);
        }
        let _producer = self.record.producer.validate(device, queue)?;
        let native = allocation
            .owner
            .device
            .as_any()
            .downcast_ref::<Device>()
            .unwrap();
        let group = native.shared.shared_queue_group.as_ref().unwrap();
        let _host = group.host[0].lock();
        if group.closed.load(Ordering::Acquire) {
            return Err(ImageLeaseError::Closed);
        }
        let mut slot = allocation.publication.lock();
        current(&slot, self.serial)?;
        let status = self.record.status.lock();
        if !ready(&status) {
            return Ok(None);
        }
        let Some(serial) = self.serial.checked_add(1) else {
            slot.phase = Phase::Abandoned;
            return Err(ImageLeaseError::Abandoned);
        };
        slot.phase = Phase::Writing {
            serial,
            producer: self.record.producer.clone(),
            alias: None,
            descriptor: None,
        };
        Ok(Some(ReturnedImage {
            inner: Some(Writing {
                lease: self.lease.clone(),
                serial,
                producer: self.record.producer.clone(),
            }),
        }))
    }
}

impl Drop for Epoch {
    fn drop(&mut self) {
        let allocation = allocation(self.lease.as_ref()).unwrap();
        let mut slot = allocation.publication.lock();
        if matches!(slot.phase, Phase::Published { serial, .. } if serial == self.serial) {
            slot.phase = Phase::Abandoned;
        }
    }
}

pub(crate) struct Payload {
    pub(crate) epoch: Arc<ImageEpoch>,
}
impl Drop for Payload {
    fn drop(&mut self) {
        let mut status = self.epoch.inner.record.status.lock();
        if status.payloads == 0 {
            status.marker = Marker::Abandoned;
        } else {
            status.payloads -= 1;
        }
    }
}

pub(crate) fn payload(epoch: Arc<ImageEpoch>) -> Result<ImagePayload, ImageLeaseError> {
    {
        let allocation = live(epoch.inner.lease.as_ref())?;
        let slot = allocation.publication.lock();
        current(&slot, epoch.inner.serial)?;
        let mut status = epoch.inner.record.status.lock();
        if status.closed {
            return Err(ImageLeaseError::Closed);
        }
        status.payloads = status
            .payloads
            .checked_add(1)
            .ok_or(ImageLeaseError::InvalidEpoch)?;
    }
    Ok(ImagePayload {
        _inner: Payload { epoch },
    })
}

pub(crate) struct Finalizing {
    lease: Arc<dyn ImageLease>,
    serial: u64,
    record: Arc<Record>,
    armed: bool,
}

impl Finalizing {
    pub(crate) fn publish(mut self) -> Arc<ImageEpoch> {
        let epoch = Arc::new(ImageEpoch {
            inner: Epoch {
                lease: self.lease.clone(),
                serial: self.serial,
                record: self.record.clone(),
            },
        });
        {
            let allocation = allocation(self.lease.as_ref()).unwrap();
            let mut slot = allocation.publication.lock();
            assert!(matches!(slot.phase, Phase::Finalizing(serial) if serial == self.serial));
            slot.phase = Phase::Published {
                serial: self.serial,
                epoch: Arc::downgrade(&epoch),
                _record: self.record.clone(),
            };
        }
        self.armed = false;
        epoch
    }
}

impl Drop for Finalizing {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let allocation = allocation(self.lease.as_ref()).unwrap();
        let mut slot = allocation.publication.lock();
        if matches!(slot.phase, Phase::Finalizing(serial) if serial == self.serial) {
            slot.phase = Phase::Abandoned;
        }
    }
}

pub(crate) struct Writing {
    pub(crate) lease: Arc<dyn ImageLease>,
    serial: u64,
    producer: Producer,
}

impl Drop for Writing {
    fn drop(&mut self) {
        let allocation = allocation(self.lease.as_ref()).unwrap();
        let mut slot = allocation.publication.lock();
        if matches!(slot.phase, Phase::Writing { serial, .. } if serial == self.serial) {
            slot.phase = Phase::Abandoned;
        }
    }
}

pub(crate) fn reserve(
    lease: Arc<dyn ImageLease>,
    device: &dyn crate::DynDevice,
    queue: &dyn crate::DynQueue,
    texture: &dyn crate::DynTexture,
    graphics: &dyn crate::DynQueue,
    descriptor: &crate::TextureDescriptor<'_>,
) -> Result<FinalizeReservation, ImageLeaseError> {
    let device = device
        .as_any()
        .downcast_ref::<Device>()
        .ok_or(ImageLeaseError::IncompatibleDevice)?;
    let queue = queue
        .as_any()
        .downcast_ref::<Queue>()
        .ok_or(ImageLeaseError::IncompatibleDevice)?;
    let texture = texture
        .as_any()
        .downcast_ref::<Texture>()
        .ok_or(ImageLeaseError::IncompatibleDevice)?;
    super::validate_alias(lease.as_ref(), device, texture, graphics, descriptor)?;
    let producer = Producer::capture(device, queue)?;
    let record = Arc::new(Record {
        descriptor: owned(descriptor),
        producer: producer.clone(),
        status: Mutex::new(Status {
            closed: false,
            payloads: 0,
            marker: Marker::None,
        }),
    });
    let serial = {
        let allocation = allocation(lease.as_ref())?;
        let mut slot = allocation.publication.lock();
        let serial = match &slot.phase {
            Phase::Fresh => 1,
            Phase::Writing {
                serial,
                producer: saved,
                alias: Some(alias),
                descriptor: Some(created),
            } if *alias == texture.identity
                && saved.same(&producer)
                && same_descriptor(created, descriptor) =>
            {
                *serial
            }
            Phase::Abandoned => return Err(ImageLeaseError::Abandoned),
            _ => return Err(ImageLeaseError::InvalidEpoch),
        };
        slot.phase = Phase::Finalizing(serial);
        serial
    };
    Ok(FinalizeReservation {
        inner: Some(Finalizing {
            lease,
            serial,
            record,
            armed: true,
        }),
    })
}

pub(crate) fn reuse(
    writing: Writing,
    device: &dyn crate::DynDevice,
    queue: &dyn crate::DynQueue,
    descriptor: &crate::TextureDescriptor<'_>,
) -> Result<Box<dyn crate::DynTexture>, ImageLeaseError> {
    let _producer = writing.producer.validate(device, queue)?;
    {
        let allocation = live(writing.lease.as_ref())?;
        let slot = allocation.publication.lock();
        if !matches!(slot.phase, Phase::Writing { serial, alias: None, .. } if serial == writing.serial)
        {
            return Err(ImageLeaseError::InvalidEpoch);
        }
    }
    let target = device
        .as_any()
        .downcast_ref::<Device>()
        .ok_or(ImageLeaseError::IncompatibleDevice)?;
    let mut alias = unsafe { super::create_alias(writing.lease.clone(), target, descriptor) }?;
    let identity = alias.as_any().downcast_ref::<Texture>().unwrap().identity;
    let valid = {
        let allocation = allocation(writing.lease.as_ref()).unwrap();
        let mut slot = allocation.publication.lock();
        match &mut slot.phase {
            Phase::Writing {
                serial,
                alias,
                descriptor: saved,
                ..
            } if *serial == writing.serial && alias.is_none() => {
                *alias = Some(identity);
                *saved = Some(owned(descriptor));
                true
            }
            _ => false,
        }
    };
    if !valid {
        unsafe { <Device as crate::DynDevice>::destroy_texture(target, alias) };
        return Err(ImageLeaseError::InvalidEpoch);
    }
    alias
        .as_any_mut()
        .downcast_mut::<Texture>()
        .unwrap()
        .drop_guard = crate::DropGuard::from_option(Some(Box::new(move || drop(writing))));
    Ok(alias)
}

#[cfg(test)]
mod tests;
