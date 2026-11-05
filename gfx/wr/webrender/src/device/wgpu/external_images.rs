/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{core, hal, wgt, Device, DmaBufImage, SharedTimeline, SyncFileWait};
use super::images::Image;
use super::native_access::{NativeAccess, NativeReturn, Prepare, ReturnKind, ReturnStatus};
use super::native_submit::{NativeSignal, NativeWait};
use api::ExternalTextureHandle;
use ash::vk;
use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::{HashMap, VecDeque},
    convert::TryFrom,
    rc::{Rc, Weak},
    sync::Arc,
};

pub struct NativeImage {
    allocation: Arc<DmaBufImage>,
    family: u32,
    access: RefCell<NativeAccess>,
    raw: OnceCell<Arc<core::resource::Texture>>,
    normal: OnceCell<Arc<Image>>,
    opaque: OnceCell<Arc<Image>>,
    released: Rc<SharedTimeline>,
    last_return: Cell<u64>,
    receipt: RefCell<Option<PendingExternalRelease>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalReleaseStatus {
    Pending,
    Submitted(u64),
    Complete,
    Abandoned,
}

struct ReturnRecord {
    owner: Arc<Device>,
    timeline: Rc<SharedTimeline>,
    state: RefCell<NativeReturn>,
    core_done: OnceCell<Arc<std::sync::atomic::AtomicBool>>,
    value: u64,
}

#[derive(Clone)]
pub struct PendingExternalRelease(Rc<ReturnRecord>);
impl PendingExternalRelease {
    pub fn value(&self) -> u64 {
        self.0.value
    }
    pub fn status(&self) -> ExternalReleaseStatus {
        let _ = self.0.owner.poll();
        let healthy = self.0.owner.failure().is_none();
        let value = self.0.timeline.current_value().map(|value| {
            if self.0.core_done.get().is_some_and(|done|done.load(std::sync::atomic::Ordering::Acquire)) {value}else{0}
        }).map_err(|_| ());
        match self.0.state.borrow_mut().status(healthy, value) {
            ReturnStatus::Pending => ExternalReleaseStatus::Pending,
            ReturnStatus::Submitted(value) => ExternalReleaseStatus::Submitted(value),
            ReturnStatus::Complete => ExternalReleaseStatus::Complete,
            ReturnStatus::Abandoned => ExternalReleaseStatus::Abandoned,
        }
    }
}

impl NativeImage {
    #[cfg(test)]
    pub(crate) fn test_native_identities(&self)->(usize,Option<usize>) {
        (Arc::as_ptr(&self.allocation) as usize,self.raw.get().map(|raw|Arc::as_ptr(raw) as usize))
    }
    pub fn has_access(&self) -> bool {
        self.access.borrow().has_access()
    }
    pub fn descriptor(&self) -> &super::DmaBufImageDescriptor {
        self.allocation.descriptor()
    }
    pub fn return_timeline(&self) -> &Rc<SharedTimeline> {
        &self.released
    }
    pub fn last_release(&self) -> Option<PendingExternalRelease> {
        self.receipt.borrow().clone()
    }

    fn wrap(&self) -> Result<(), String> {
        let owner = &self.allocation.owner;
        let d = self.descriptor();
        let mut usage = wgt::TextureUsages::TEXTURE_BINDING;
        if d.usage.contains(wgt::TextureUses::COPY_SRC) {
            usage |= wgt::TextureUsages::COPY_SRC;
        }
        let desc = core::resource::TextureDescriptor {
            label: Some("WR initialized native image".into()),
            size: wgt::Extent3d {
                width: d.size[0],
                height: d.size[1],
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format: d.format,
            usage,
            view_formats: Vec::new(),
        };
        let (raw, error) = unsafe {
            owner.device.create_texture_from_hal(
                self.allocation.hal_texture(),
                &desc,
                wgt::TextureUses::RESOURCE,
                true,
            )
        };
        if let Some(error) = error {
            return Err(error.to_string());
        }
        owner.check().map_err(|e| e.to_string())?;
        self.access.borrow_mut().wrapped().map_err(str::to_owned)?;
        self.raw
            .set(raw)
            .map_err(|_| "Native image wrapped twice")?;
        Ok(())
    }
    fn image(&self, opaque: bool) -> Result<Arc<Image>, String> {
        let cache = if opaque { &self.opaque } else { &self.normal };
        if let Some(image) = cache.get() {
            return Ok(image.clone());
        }
        let owner = &self.allocation.owner;
        if opaque
            && !owner
                .features()
                .contains(wgt::Features::TEXTURE_COMPONENT_SWIZZLE)
        {
            return Err("Opaque native sampling needs component swizzle support".into());
        }
        let raw = self
            .raw
            .get()
            .ok_or("Native image initialization is pending")?
            .clone();
        let sampled = raw.create_view(&core::resource::TextureViewDescriptor {
            swizzle: if opaque {
                wgt::TextureComponentSwizzle {
                    a: wgt::ComponentSwizzle::One,
                    ..Default::default()
                }
            } else {
                Default::default()
            },
            ..Default::default()
        });
        owner.check().map_err(|e| e.to_string())?;
        let image = Arc::new(Image {
            raw,
            sampled: sampled.clone(),
            levels: vec![sampled],
            filter: super::TextureFilter::Linear,
        });
        let _ = cache.set(image.clone());
        Ok(image)
    }
}

struct Operation {
    image: Rc<NativeImage>,
    initial: bool,
    release: Option<Weak<ReturnRecord>>,
}
impl Drop for Operation {
    fn drop(&mut self) {
        if let Some(receipt) = self.release.as_ref().and_then(Weak::upgrade) {
            receipt.state.borrow_mut().cancel_recording();
        }
    }
}
struct InFlight {
    point: u64,
    submission: core::SubmissionIndex,
    core_done: Arc<std::sync::atomic::AtomicBool>,
    operations: Vec<Operation>,
    _waits: Vec<(Rc<SharedTimeline>, u64)>,
    _binary_waits: Vec<Arc<SyncFileWait>>,
    _signals: Vec<(Rc<SharedTimeline>, u64)>,
    _retirement: Rc<SharedTimeline>,
}
struct View {
    image: Rc<NativeImage>,
    opaque: bool,
}

pub struct ExternalTextureRegistry {
    owner: Arc<Device>,
    views: RefCell<HashMap<u32, View>>,
    next_id: Cell<u32>,
    batch: Cell<u64>,
    operations: RefCell<Vec<Operation>>,
    commands: RefCell<Vec<Arc<core::command::CommandBuffer>>>,
    waits: RefCell<Vec<(Rc<SharedTimeline>, u64)>>,
    binary_waits: RefCell<Vec<Arc<SyncFileWait>>>,
    signals: RefCell<Vec<(Rc<SharedTimeline>, u64)>>,
    retirement: Rc<SharedTimeline>,
    next_point: Cell<u64>,
    in_flight: RefCell<VecDeque<InFlight>>,
    attached: Cell<bool>,
    frame_active: Cell<bool>,
    checkpoint: Cell<bool>,
}

thread_local! {static RETIRED:RefCell<Vec<Rc<ExternalTextureRegistry>>>=const{RefCell::new(Vec::new())};}
thread_local! {static ACTIVE:RefCell<Vec<Weak<ExternalTextureRegistry>>>=const{RefCell::new(Vec::new())};}

pub fn poll_retired_native_images() -> bool {
    let live = ACTIVE.with(|active| {
        let live = active
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|context| context.attached.get())
            .collect::<Vec<_>>();
        active.borrow_mut().retain(|weak| weak.strong_count() != 0);
        live
    });
    for context in &live {
        let _ = context.flush_idle();
        let _ = context.poll();
    }
    let pending = live.iter().any(|context| context.pending_work());
    pending
        | RETIRED.with(|retired| {
            let contexts = retired.borrow().clone();
            for context in &contexts {
                let _ = context.flush_idle();
                let _ = context.poll();
            }
            retired
                .borrow_mut()
                .retain(|context| !context.in_flight.borrow().is_empty());
            contexts.iter().any(|context| context.pending_work())
        })
}

/// Called after hosts are cleared, before render-thread callback infrastructure is torn down.
pub fn shutdown_native_images() {
    let mut contexts = ACTIVE.with(|active| {
        active
            .borrow_mut()
            .drain(..)
            .filter_map(|weak| weak.upgrade())
            .collect::<Vec<_>>()
    });
    RETIRED.with(|retired| contexts.extend(retired.borrow_mut().drain(..)));
    contexts.sort_unstable_by_key(Rc::as_ptr);
    contexts.dedup_by(|a, b| Rc::ptr_eq(a, b));
    for context in contexts {
        context.attached.set(false);
        context.frame_active.set(false);
        context.abort();
        let _ = context.finalize_native();
    }
}

impl Drop for ExternalTextureRegistry {
    fn drop(&mut self) {
        if self.in_flight.get_mut().is_empty() {
            return;
        }
        if self.owner.native_idle().is_ok() {
            self.in_flight.get_mut().clear();
        } else {
            log::error!("Native Vulkan retirement could not prove idle; quarantining {} native batches until process exit", self.in_flight.get_mut().len());
            // Only an indeterminate native wait failure reaches this last-resort
            // process-lifetime quarantine; batches contain no host or callback.
            for batch in self.in_flight.get_mut().drain(..) {
                std::mem::forget(batch);
            }
        }
    }
}

impl ExternalTextureRegistry {
    pub(super) fn new(owner: &Arc<Device>) -> Result<Rc<Self>, String> {
        let context = Rc::new(Self {
            owner: owner.clone(),
            views: RefCell::new(HashMap::new()),
            next_id: Cell::new(0),
            batch: Cell::new(1),
            operations: RefCell::new(Vec::new()),
            commands: RefCell::new(Vec::new()),
            waits: RefCell::new(Vec::new()),
            binary_waits: RefCell::new(Vec::new()),
            signals: RefCell::new(Vec::new()),
            retirement: SharedTimeline::new(owner)?,
            next_point: Cell::new(0),
            in_flight: RefCell::new(VecDeque::new()),
            attached: Cell::new(true),
            frame_active: Cell::new(false),
            checkpoint: Cell::new(false),
        });
        ACTIVE.with(|active| active.borrow_mut().push(Rc::downgrade(&context)));
        Ok(context)
    }
    #[cfg(test)]
    pub(crate) fn test_defer_core_completion(&self) -> Arc<std::sync::atomic::AtomicBool> {
        let deferred = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.in_flight.borrow_mut().front_mut().unwrap().core_done = deferred.clone();
        deferred
    }
    #[cfg(test)]
    pub(crate) fn test_native_signals_complete(&self) -> bool {
        self.in_flight.borrow().iter().all(|batch| {
            batch._signals.iter().all(|(timeline, value)| {
                timeline
                    .current_value()
                    .is_ok_and(|current| current >= *value)
            })
        })
    }
    pub fn device(&self) -> &Arc<Device> {
        &self.owner
    }
    pub fn is_attached(&self) -> bool {
        self.attached.get()
    }
    pub fn matches(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
    pub fn pending_work(&self) -> bool {
        !self.operations.borrow().is_empty() || !self.in_flight.borrow().is_empty()
    }
    pub(super) fn recording_work(&self) -> bool {
        !self.operations.borrow().is_empty() || self.checkpoint.get()
    }
    pub(super) fn begin_frame(&self) {
        self.frame_active.set(true);
    }
    pub(super) fn end_frame(&self) {
        self.frame_active.set(false);
    }
    pub fn import(&self, allocation:Arc<DmaBufImage>)->Result<Rc<NativeImage>,String> {
        self.import_with_family(allocation,vk::QUEUE_FAMILY_EXTERNAL)
    }
    pub fn import_foreign(&self, allocation:Arc<DmaBufImage>)->Result<Rc<NativeImage>,String> {
        self.import_with_family(allocation,vk::QUEUE_FAMILY_FOREIGN_EXT)
    }
    fn import_with_family(&self, allocation: Arc<DmaBufImage>, family:u32) -> Result<Rc<NativeImage>, String> {
        if !Arc::ptr_eq(&self.owner, &allocation.owner) {
            return Err("Native allocation belongs to another device".into());
        }
        Ok(Rc::new(NativeImage {
            allocation,
            family,
            access: RefCell::new(unsafe { NativeAccess::initialized_publication() }),
            raw: OnceCell::new(),
            normal: OnceCell::new(),
            opaque: OnceCell::new(),
            released: SharedTimeline::new(&self.owner)?,
            last_return: Cell::new(0),
            receipt: RefCell::new(None),
        }))
    }
    pub fn register(
        &self,
        image: &Rc<NativeImage>,
        opaque: bool,
    ) -> Result<ExternalTextureHandle, String> {
        if !Arc::ptr_eq(&self.owner, &image.allocation.owner) {
            return Err("Native image device mismatch".into());
        }
        if opaque
            && !self
                .owner
                .features()
                .contains(wgt::Features::TEXTURE_COMPONENT_SWIZZLE)
        {
            return Err("Opaque native sampling unavailable".into());
        }
        let id = self
            .next_id
            .get()
            .checked_add(1)
            .ok_or("Native texture names exhausted")?;
        self.next_id.set(id);
        self.views.borrow_mut().insert(
            id,
            View {
                image: image.clone(),
                opaque,
            },
        );
        Ok(ExternalTextureHandle(u64::from(id)))
    }
    pub fn unregister(&self, handle: ExternalTextureHandle) -> Result<(), String> {
        self.views
            .borrow_mut()
            .remove(&u32::try_from(handle.0).map_err(|_| "Invalid native texture name")?)
            .ok_or("Unknown native texture")?;
        Ok(())
    }
    #[cfg(feature = "capture")]
    pub(super) fn is_opaque(&self, id: u32) -> Result<bool, String> {
        self.views
            .borrow()
            .get(&id)
            .map(|view| view.opaque)
            .ok_or_else(|| "Unknown native texture".into())
    }
    pub(super) fn image(&self, id: u32) -> Result<Arc<Image>, String> {
        let views = self.views.borrow();
        let view = views.get(&id).ok_or("Unknown native texture")?;
        if !view.image.access.borrow().may_sample(self.batch.get()) {
            return Err("Native image is outside acquired access".into());
        }
        view.image.image(view.opaque)
    }
    /// Ready covers fully initialized producer pixels and GENERAL/EXTERNAL release.
    /// The producer and all aliases wait for the returned timeline before reuse.
    pub unsafe fn acquire(
        &self,
        image: &Rc<NativeImage>,
        ready: &Rc<SharedTimeline>,
        value: u64,
    ) -> Result<bool, String> {
        if !self.attached.get() {
            return Err("Native renderer is retired".into());
        }
        self.poll()?;
        if image.family!=vk::QUEUE_FAMILY_EXTERNAL {return Err("Expected Vulkan EXTERNAL image".into());}
        if !Arc::ptr_eq(&self.owner, &ready.owner) {
            return Err("Native ready timeline must belong to this device".into());
        }
        ready.validate_wait(value)?;
        // Do not stall the window's queue on an unknown producer; the actual GPU wait
        // is still staged below once the ready value is observed.
        if ready.current_value()? < value {
            return Ok(false);
        }
        let action=self.prepare_image(image)?;
        match action {
            Prepare::RecordFirstAcquire | Prepare::RecordAcquire => {
                let initial = action == Prepare::RecordFirstAcquire;
                let commands = self.barrier(image, true)?;
                self.commands.borrow_mut().push(commands);
                self.waits.borrow_mut().push((ready.clone(), value));
                self.operations.borrow_mut().push(Operation {
                    image: image.clone(),
                    initial,
                    release: None,
                });
                Ok(!initial)
            }
            Prepare::Pending => Ok(false),
            Prepare::Ready => Ok(true),
            Prepare::WrapInitialized => unreachable!(),
        }
    }
    fn prepare_image(&self,image:&Rc<NativeImage>)->Result<Prepare,String> {
        if !Arc::ptr_eq(&self.owner,&image.allocation.owner) {return Err("Native image belongs to another device".into());}
        let mut action = image
            .access
            .borrow_mut()
            .prepare(self.batch.get())
            .map_err(str::to_owned)?;
        if action == Prepare::WrapInitialized {
            image.wrap()?;
            action = image
                .access
                .borrow_mut()
                .prepare(self.batch.get())
                .map_err(str::to_owned)?;
        }
        Ok(action)
    }
    /// Ready must be a genuine fence covering fully initialized immutable producer pixels.
    pub unsafe fn acquire_foreign(&self,image:&Rc<NativeImage>,ready:std::os::fd::BorrowedFd<'_>)->Result<bool,String> {
        if !self.attached.get(){return Err("Native renderer is retired".into());}
        self.poll()?;
        if image.family!=vk::QUEUE_FAMILY_FOREIGN_EXT{return Err("Expected GL FOREIGN image".into());}
        if !SyncFileWait::ready(ready)?{return Ok(false);}
        let action=self.prepare_image(image)?;
        match action {
            Prepare::RecordFirstAcquire|Prepare::RecordAcquire=>{
                let result=(||{
                    let wait=unsafe{SyncFileWait::import(&self.owner,ready)}?;
                    let commands=self.barrier(image,true)?;
                    self.commands.borrow_mut().push(commands);
                    self.binary_waits.borrow_mut().push(wait);
                    self.operations.borrow_mut().push(Operation{image:image.clone(),initial:action==Prepare::RecordFirstAcquire,release:None});
                    Ok(action!=Prepare::RecordFirstAcquire)
                })();
                if result.is_err(){image.access.borrow_mut().abandon();}
                result
            },
            Prepare::Pending=>Ok(false),Prepare::Ready=>Ok(true),Prepare::WrapInitialized=>unreachable!(),
        }
    }
    pub fn release(&self, image: &Rc<NativeImage>) -> Result<PendingExternalRelease, String> {
        image
            .access
            .borrow_mut()
            .release(self.batch.get())
            .map_err(str::to_owned)?;
        let value = image
            .last_return
            .get()
            .checked_add(1)
            .ok_or("Native return points exhausted")?;
        image.released.validate_signal(&self.owner, value)?;
        let receipt = PendingExternalRelease(Rc::new(ReturnRecord {
            owner: self.owner.clone(),
            timeline: image.released.clone(),
            state: RefCell::new(
                NativeReturn::recording(if image.family==vk::QUEUE_FAMILY_FOREIGN_EXT{ReturnKind::Foreign}else{ReturnKind::Vulkan}, value).map_err(str::to_owned)?,
            ),
            value,
            core_done: OnceCell::new(),
        }));
        let commands = self.barrier(image, false)?;
        self.commands.borrow_mut().push(commands);
        self.signals
            .borrow_mut()
            .push((image.released.clone(), value));
        self.operations.borrow_mut().push(Operation {
            image: image.clone(),
            initial: false,
            release: Some(Rc::downgrade(&receipt.0)),
        });
        image.last_return.set(value);
        *image.receipt.borrow_mut() = Some(receipt.clone());
        Ok(receipt)
    }
    fn barrier(
        &self,
        image: &Rc<NativeImage>,
        acquire: bool,
    ) -> Result<Arc<core::command::CommandBuffer>, String> {
        let encoder = self
            .owner
            .create_command_encoder("WR native ownership")
            .map_err(|e| e.to_string())?;
        let hal = self.owner.raw_device();
        let raw = hal.raw_device();
        let family = hal.queue_family_index();
        let source = image.allocation.image;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let general = vk::ImageLayout::GENERAL;
        let sampled = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
        let ignored = vk::QUEUE_FAMILY_IGNORED;
        let external = image.family;
        let read = vk::AccessFlags::SHADER_READ;
        let empty = vk::AccessFlags::empty();
        let barriers = if acquire {
            [
                (general, general, external, family, empty, read),
                (general, sampled, ignored, ignored, empty, read),
            ]
        } else {
            [
                (sampled, general, ignored, ignored, read, empty),
                (general, general, family, external, read, empty),
            ]
        };
        unsafe {
            encoder.as_hal_mut::<hal::api::Vulkan, _, _>(|hal| {
                let command = hal.expect("Vulkan native encoder").raw_handle();
                for (old, new, src, dst, src_access, dst_access) in barriers {
                    raw.cmd_pipeline_barrier(
                        command,
                        vk::PipelineStageFlags::ALL_COMMANDS,
                        vk::PipelineStageFlags::ALL_COMMANDS,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[vk::ImageMemoryBarrier::default()
                            .image(source)
                            .subresource_range(range)
                            .old_layout(old)
                            .new_layout(new)
                            .src_queue_family_index(src)
                            .dst_queue_family_index(dst)
                            .src_access_mask(src_access)
                            .dst_access_mask(dst_access)],
                    );
                }
            });
        }
        Ok(encoder.finish(&wgt::CommandBufferDescriptor::default()))
    }
    pub(super) fn take_commands(&self) -> Vec<Arc<core::command::CommandBuffer>> {
        std::mem::take(&mut *self.commands.borrow_mut())
    }
    pub(super) fn submit(
        &self,
        commands: &[Arc<core::command::CommandBuffer>],
    ) -> Result<core::SubmissionIndex, String> {
        let checkpoint = self.checkpoint.replace(false);
        if self.operations.borrow().is_empty() && !checkpoint {
            return self
                .owner
                .submit(commands)
                .map_err(|error| error.to_string());
        }
        let batch = self.batch.get();
        let next_batch = batch
            .checked_add(1)
            .ok_or("Native recording identifiers exhausted")?;
        let point = self
            .next_point
            .get()
            .checked_add(1)
            .ok_or("Native retirement points exhausted")?;
        let validation = (|| {
            for (timeline, value) in self.waits.borrow().iter() {
                timeline.validate_wait(*value)?;
            }
            for (timeline, value) in self.signals.borrow().iter() {
                timeline.validate_signal(&self.owner, *value)?;
            }
            self.retirement.validate_signal(&self.owner, point)
        })();
        if let Err(error) = validation {
            self.abort();
            return Err(error);
        }
        let operations = std::mem::take(&mut *self.operations.borrow_mut());
        let waits = std::mem::take(&mut *self.waits.borrow_mut());
        let binary_waits=std::mem::take(&mut *self.binary_waits.borrow_mut());
        let mut signals = std::mem::take(&mut *self.signals.borrow_mut());
        let outcome = unsafe {
            self.owner.submit_native(
                commands,
                &waits
                    .iter()
                    .map(|(timeline, value)| NativeWait {
                        semaphore: timeline.semaphore,
                        value: Some(*value),
                    })
                    .chain(binary_waits.iter().map(|wait|NativeWait{semaphore:wait.semaphore,value:None}))
                    .collect::<Vec<_>>(),
                &signals
                    .iter()
                    .map(|(timeline, value)| NativeSignal {
                        semaphore: timeline.semaphore,
                        value: *value,
                    })
                    .collect::<Vec<_>>(),
                NativeSignal {
                    semaphore: self.retirement.semaphore,
                    value: point,
                },
            )
        };
        self.batch.set(next_batch);
        self.next_point.set(point);
        let core_done=outcome.core_done.clone();
        let submitted_index = outcome.result.as_ref().ok().copied();
        let mut result = outcome.result.map_err(|error| error.to_string());
        let mut unique = HashMap::new();
        for operation in &operations {
            unique.insert(Rc::as_ptr(&operation.image), operation.image.clone());
        }
        if let Some(index) = submitted_index {
            for (timeline, value) in &signals {
                timeline.signal_submitted(*value);
            }
            self.retirement.signal_submitted(point);
            let commit = (|| {
                for image in unique.values() {
                    image
                        .access
                        .borrow_mut()
                        .submitted(batch, index)
                        .map_err(str::to_owned)?;
                }
                for operation in &operations {
                    if let Some(receipt) = operation.release.as_ref().and_then(Weak::upgrade) {
                        receipt
                            .state
                            .borrow_mut()
                            .submitted()
                            .map_err(str::to_owned)?;
                        let _=receipt.core_done.set(core_done.clone());
                    }
                }
                Ok(())
            })();
            if let Err(error) = commit {
                result = Err(error);
            }
        }
        if result.is_err() {
            for image in unique.values() {
                image.access.borrow_mut().abandon();
            }
            for operation in &operations {
                if let Some(receipt) = operation.release.as_ref().and_then(Weak::upgrade) {
                    receipt.state.borrow_mut().abandon();
                }
            }
        }
        // No fallible operation after the native submit can bypass native retention.
        if outcome.may_be_in_flight {
            signals.push((self.retirement.clone(), point));
            self.in_flight.borrow_mut().push_back(InFlight {
                point,
                submission: submitted_index.unwrap_or(0),
                core_done: outcome.core_done,
                operations,
                _waits: waits,
                _binary_waits: binary_waits,
                _signals: signals,
                _retirement: self.retirement.clone(),
            });
        }
        result
    }
    pub fn poll(&self) -> Result<(), String> {
        let polled = self.owner.poll().map_err(|e| e.to_string());
        if polled.is_err() {
            for batch in self.in_flight.borrow().iter() {
                for operation in &batch.operations {
                    operation.image.access.borrow_mut().abandon();
                    if let Some(receipt) = operation.release.as_ref().and_then(Weak::upgrade) {
                        receipt.state.borrow_mut().abandon();
                    }
                }
            }
        }
        let value = match self.retirement.current_value() {
            Ok(value) => value,
            Err(error) => {
                if self.owner.native_device_lost() {
                    self.finalize_native()?;
                }
                return Err(error);
            }
        };
        if self
            .in_flight
            .borrow()
            .iter()
            .any(|batch| batch.submission == 0)
        {
            self.finalize_native()?;
            return polled.map(|_| ());
        }
        let mut in_flight = self.in_flight.borrow_mut();
        loop {
            let Some(batch) = in_flight.front() else {
                break;
            };
            if batch.point > value || !batch.core_done.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            let mut signals_complete = true;
            for (timeline, point) in &batch._signals {
                signals_complete &= timeline.current_value()? >= *point;
            }
            if !signals_complete {
                break;
            }
            let batch = in_flight.pop_front().unwrap();
            if polled.is_ok() {
                for operation in batch.operations {
                    if operation.initial && batch.submission != 0 {
                        operation
                            .image
                            .access
                            .borrow_mut()
                            .observe_initial_completion(batch.submission);
                    }
                }
            }
        }
        polled.map(|_| ())
    }
    fn finalize_native(&self) -> Result<(), String> {
        if self.in_flight.borrow().is_empty() {
            return Ok(());
        }
        // A logical core destroy is not completion. Vulkan idle SUCCESS/DEVICE_LOST
        // establishes destruction safety, never a producer ownership return.
        self.owner.native_idle()?;
        self.in_flight.borrow_mut().clear();
        let _ = self.owner.poll();
        Ok(())
    }
    /// Explicit canvas snapshots only; restore sampling before ownership release.
    pub fn readback(&self, image: &Rc<NativeImage>, output: &mut [u8]) -> Result<(), String> {
        if self.frame_active.get() || !image.access.borrow().may_sample(self.batch.get()) {
            return Err("Native snapshot is outside acquired idle access".into());
        }
        let source = image.image(false)?;
        let desc = source.raw.descriptor();
        let length = (desc.size.width as usize).checked_mul(desc.size.height as usize).and_then(|n| n.checked_mul(4));
        if length != Some(output.len()) { return Err("Native snapshot output size mismatch".into()); }
        let encoder = self.owner.create_command_encoder("Native canvas snapshot").map_err(|e| e.to_string())?;
        let rect = api::units::DeviceIntRect::from_size(api::units::DeviceIntSize::new(desc.size.width as i32, desc.size.height as i32));
        let readback = self.owner.encode_readback(&encoder, &source.raw, 0, rect, None).map_err(|e| e.to_string())?;
        encoder.transition_resources(std::iter::empty(), std::iter::once(wgt::TextureTransition {
            texture: source.raw.clone(), selector: None, state: wgt::TextureUses::RESOURCE,
        }));
        let mut commands = self.take_commands();
        commands.push(encoder.finish(&wgt::CommandBufferDescriptor::default()));
        let index = self.submit(&commands)?;
        readback.map(&self.owner, index).map_err(|e| e.to_string())?.wait_into(&self.owner, output).map_err(|e| e.to_string())
    }
    pub fn flush_idle(&self) -> Result<(), String> {
        if !self.frame_active.get() && !self.operations.borrow().is_empty() {
            let commands = self.take_commands();
            self.submit(&commands)?;
        }
        Ok(())
    }
    pub(super) fn abort(&self) {
        for view in self.views.borrow().values() {
            view.image.access.borrow_mut().abandon();
        }
        for operation in self.operations.borrow().iter() {
            operation.image.access.borrow_mut().abandon();
        }
        self.operations.borrow_mut().clear();
        self.commands.borrow_mut().clear();
        self.waits.borrow_mut().clear();
        self.binary_waits.borrow_mut().clear();
        self.signals.borrow_mut().clear();
    }
    pub(super) fn close(&self) {
        self.attached.set(false);
        if self.views.borrow().is_empty() && !self.pending_work() {
            return;
        }
        self.checkpoint.set(true);
        let mut images = HashMap::new();
        for view in self.views.borrow().values() {
            images.insert(Rc::as_ptr(&view.image), view.image.clone());
        }
        for image in images.values() {
            let _ = self.release(image);
        }
    }
    pub(super) fn retire(self: &Rc<Self>) {
        self.frame_active.set(false);
        self.attached.set(false);
        let _ = self.flush_idle();
        let _ = self.poll();
        if !self.in_flight.borrow().is_empty() {
            RETIRED.with(|retired| retired.borrow_mut().push(self.clone()));
        }
    }
}
