/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::unified::ProcessOwner;
use std::sync::Arc;
use wgc::{
    device::Device,
    resource::{SealedCanvasTexture, SharedCanvasTexture, Texture, TextureDescriptor},
    SubmissionIndex,
};
use wgh::shared_image::{AllocationBudget, ImageEpoch};

pub struct NativeCanvasAllocator {
    owner: Arc<ProcessOwner>,
    budget: Arc<AllocationBudget>,
}

impl NativeCanvasAllocator {
    pub fn new(owner: Arc<ProcessOwner>) -> Self {
        Self {
            owner,
            budget: Arc::new(AllocationBudget::new(128)),
        }
    }

    pub fn create_texture(
        &self,
        device: &Arc<Device>,
        descriptor: &TextureDescriptor<'_>,
    ) -> Result<(Arc<Texture>, Option<SharedCanvasTexture>), String> {
        if !self.owner.is_ready() {
            return Err("Native canvas owner is unavailable for allocation".into());
        }
        Ok(unsafe {
            device.create_shared_canvas_texture(
                descriptor,
                self.owner.graphics(),
                self.budget.clone(),
            )
        })
    }

    pub fn live_allocations(&self) -> usize {
        self.budget.live_allocations()
    }
}

pub struct SealedCanvasFrame {
    owner: Arc<ProcessOwner>,
    epoch: Arc<ImageEpoch>,
    logical_descriptor: wgt::TextureDescriptor<String, Vec<wgt::TextureFormat>>,
    submission_index: SubmissionIndex,
}

impl SealedCanvasFrame {
    pub fn new(owner: Arc<ProcessOwner>, sealed: SealedCanvasTexture) -> Result<Arc<Self>, String> {
        let submission_index = sealed.submission_index();
        let (finalized, logical_descriptor) = sealed.into_parts();
        let epoch = finalized.publish_epoch();
        let graphics = owner.graphics();
        epoch
            .validate_owner(graphics.device.as_ref(), graphics.queue.as_ref())
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(Self {
            owner,
            epoch,
            logical_descriptor,
            submission_index,
        }))
    }

    pub fn owner(&self) -> &Arc<ProcessOwner> {
        &self.owner
    }
    pub fn epoch(&self) -> &Arc<ImageEpoch> {
        &self.epoch
    }
    pub fn logical_descriptor(&self) -> &wgt::TextureDescriptor<String, Vec<wgt::TextureFormat>> {
        &self.logical_descriptor
    }
    pub fn native_descriptor(&self) -> &wgh::TextureDescriptor<'static> {
        self.epoch.descriptor()
    }
    pub fn submission_index(&self) -> SubmissionIndex {
        self.submission_index
    }
}

pub struct NativeCanvasFrame {
    frame: Arc<SealedCanvasFrame>,
}

impl NativeCanvasFrame {
    pub fn new(frame: Arc<SealedCanvasFrame>) -> Self {
        Self { frame }
    }
    pub fn frame(&self) -> &Arc<SealedCanvasFrame> {
        &self.frame
    }
}

/// # Safety
/// The pointer must be null or an owned box returned for this opaque frame type.
#[no_mangle]
pub unsafe extern "C" fn wgpu_native_canvas_frame_delete(frame: *mut NativeCanvasFrame) {
    if !frame.is_null() {
        drop(unsafe { Box::from_raw(frame) });
    }
}

use crate::error::GPUError;
use crate::NativeCanvasSnapshotKind;
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::Weak,
};
use wgc::{device::queue::Queue, resource::ParentDevice as _};
use wgpu_core_remote::{global::Global as CoreGlobal, id};
use wgpu_core_remote_types::TextureDescriptor as CanvasDescriptor;

#[derive(Clone)]
pub struct NativeConfiguration {
    pub generation: u64,
    pub device_id: id::DeviceId,
    pub queue_id: id::QueueId,
    pub descriptor: CanvasDescriptor<'static>,
    pub opaque: bool,
}

enum Current {
    Absent,
    Writing {
        id: id::TextureId,
        canvas: SharedCanvasTexture,
    },
    Sealed {
        id: id::TextureId,
        frame: Arc<SealedCanvasFrame>,
    },
    Error {
        id: id::TextureId,
    },
    InProgress {
        id: id::TextureId,
    },
    PublishingBlank,
}

impl Current {
    fn id(&self) -> Option<id::TextureId> {
        match self {
            Self::Absent | Self::PublishingBlank => None,
            Self::Writing { id, .. }
            | Self::Sealed { id, .. }
            | Self::Error { id }
            | Self::InProgress { id } => Some(*id),
        }
    }
}

struct Configuration {
    info: NativeConfiguration,
    device: Weak<Device>,
    queue: Weak<Queue>,
    registered: bool,
    native_supported: bool,
    current: Current,
    last_publication: Option<(u64, bool)>,
}

struct Configurations {
    next_generation: u64,
    owners: HashMap<u64, Configuration>,
}

pub struct NativeCanvases {
    allocator: NativeCanvasAllocator,
    configs: RefCell<Configurations>,
    protocol_error: Cell<bool>,
}

pub struct NativePublication {
    pub generation: u64,
    pub texture_id: Option<id::TextureId>,
    pub remote_id: u64,
    pub frame: Option<Arc<SealedCanvasFrame>>,
    pub submission_index: SubmissionIndex,
}

fn validation(device: &Arc<Device>, message: impl Into<String>, operation: &'static str) {
    device.handle_error(
        GPUError {
            message: Cow::Owned(message.into()),
            r#type: wgt::error::ErrorType::Validation,
        },
        None,
        operation,
    );
}

impl NativeCanvases {
    pub fn new(owner: Arc<ProcessOwner>) -> Self {
        Self {
            allocator: NativeCanvasAllocator::new(owner),
            configs: RefCell::new(Configurations {
                next_generation: 0,
                owners: HashMap::new(),
            }),
            protocol_error: Cell::new(false),
        }
    }

    pub fn protocol_error(&self) -> bool {
        self.protocol_error.get()
    }

    pub fn reject_protocol(&self) {
        self.protocol_error.set(true);
    }

    pub fn live_allocations(&self) -> usize {
        self.allocator.live_allocations()
    }

    pub fn configure(
        &self,
        global: &CoreGlobal,
        owner: u64,
        device_id: id::DeviceId,
        queue_id: id::QueueId,
        descriptor: &CanvasDescriptor<'_>,
        opaque: bool,
    ) -> Option<NativeConfiguration> {
        let Some((device, queue)) = global.resolve_canvas_device_queue(device_id, queue_id) else {
            self.reject_protocol();
            return None;
        };
        let descriptor: CanvasDescriptor<'static> = descriptor.map_label(|_| None);
        let native_supported = matches!(
            descriptor.format,
            wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm
        ) && !descriptor
            .usage
            .contains(wgt::TextureUsages::TRANSIENT_ATTACHMENT);
        let info = {
            let mut configs = self.configs.borrow_mut();
            if owner == 0 || configs.owners.contains_key(&owner) {
                self.reject_protocol();
                return None;
            }
            let Some(generation) = configs.next_generation.checked_add(1) else {
                self.reject_protocol();
                return None;
            };
            configs.next_generation = generation;
            let info = NativeConfiguration {
                generation,
                device_id,
                queue_id,
                descriptor,
                opaque,
            };
            configs.owners.insert(
                owner,
                Configuration {
                    info: info.clone(),
                    device: Arc::downgrade(&device),
                    queue: Arc::downgrade(&queue),
                    registered: false,
                    native_supported,
                    current: Current::Absent,
                    last_publication: None,
                },
            );
            info
        };
        if let Err(error) = device.validate_texture_descriptor(&info.descriptor) {
            device.handle_error(error, None, "GPUCanvasContext.configure");
        }
        if !native_supported {
            validation(
                &device,
                "Unsupported native canvas configuration",
                "GPUCanvasContext.configure",
            );
        }
        Some(info)
    }

    pub fn configured(&self, owner: u64, generation: u64, registered: bool) {
        let mut configs = self.configs.borrow_mut();
        if let Some(config) = configs
            .owners
            .get_mut(&owner)
            .filter(|config| config.info.generation == generation)
        {
            config.registered = registered;
            if !registered {
                self.reject_protocol();
            }
        } else {
            self.reject_protocol();
        }
    }

    pub fn create_current(
        &self,
        global: &CoreGlobal,
        owner: u64,
        device_id: id::DeviceId,
        texture_id: id::TextureId,
    ) {
        if !global.canvas_texture_id_available(texture_id) {
            self.reject_protocol();
            return;
        }
        let prepared = {
            let mut configs = self.configs.borrow_mut();
            let Some(config) = configs.owners.get_mut(&owner) else {
                self.reject_protocol();
                return;
            };
            if !config.registered
                || config.info.device_id != device_id
                || !matches!(config.current, Current::Absent)
            {
                self.reject_protocol();
                return;
            }
            let Some(device) = config.device.upgrade() else {
                self.reject_protocol();
                return;
            };
            config.current = Current::InProgress { id: texture_id };
            (config.info.clone(), device, config.native_supported)
        };
        let (info, device, supported) = prepared;
        let (texture, canvas, admission_error) = if supported {
            match self.allocator.create_texture(&device, &info.descriptor) {
                Ok((texture, canvas)) => (texture, canvas, None),
                Err(error) => (
                    device.create_texture_error(&info.descriptor),
                    None,
                    Some(error),
                ),
            }
        } else {
            (device.create_texture_error(&info.descriptor), None, None)
        };
        if !global.register_canvas_texture(texture_id, texture) {
            self.reject_protocol();
            if let Some(canvas) = canvas {
                canvas.texture().destroy();
            }
            return;
        }
        let current = match canvas {
            Some(canvas) => Current::Writing {
                id: texture_id,
                canvas,
            },
            None => Current::Error { id: texture_id },
        };
        self.restore_current(owner, info.generation, texture_id, current);
        if let Some(error) = admission_error {
            validation(&device, error, "GPUCanvasContext.getCurrentTexture");
        }
    }

    fn restore_current(&self, owner: u64, generation: u64, id: id::TextureId, current: Current) {
        let discarded = {
            let mut configs = self.configs.borrow_mut();
            if let Some(config) = configs.owners.get_mut(&owner).filter(|config|
                config.info.generation == generation &&
                matches!(config.current, Current::InProgress { id: pending } if pending == id))
            {
                config.current = current;
                None
            } else { Some(current) }
        };
        if let Some(Current::Writing { canvas, .. }) = discarded {
            canvas.texture().destroy();
        }
    }

    fn take_writing(
        &self,
        owner: u64,
    ) -> Option<(
        NativeConfiguration,
        id::TextureId,
        SharedCanvasTexture,
        Option<Arc<Queue>>,
    )> {
        let mut configs = self.configs.borrow_mut();
        let config = configs.owners.get_mut(&owner)?;
        let id = config.current.id()?;
        if !matches!(config.current, Current::Writing { .. }) {
            return None;
        }
        let Current::Writing { canvas, .. } =
            std::mem::replace(&mut config.current, Current::InProgress { id })
        else {
            unreachable!()
        };
        Some((config.info.clone(), id, canvas, config.queue.upgrade()))
    }

    fn seal_current(&self, owner: u64, report_error: bool) {
        let Some((info, id, canvas, queue)) = self.take_writing(owner) else {
            return;
        };
        let texture = canvas.texture().clone();
        let device = texture.device().clone();
        enum SealError {
            Core(wgc::device::DeviceError),
            Admission(String),
        }
        let result = match queue {
            Some(queue) => unsafe { queue.seal_shared_canvas(canvas) }
                .map_err(SealError::Core)
                .and_then(|sealed| {
                    SealedCanvasFrame::new(self.allocator.owner.clone(), sealed)
                        .map_err(SealError::Admission)
                }),
            None => Err(SealError::Admission(
                "Native canvas queue is unavailable".into(),
            )),
        };
        let current = match result {
            Ok(frame) => Current::Sealed { id, frame },
            Err(error) => {
                texture.destroy();
                if report_error {
                    match error {
                        SealError::Core(error) => {
                            device.handle_error(error, None, "GPUCanvasContext native seal")
                        }
                        SealError::Admission(error) => {
                            validation(&device, error, "GPUCanvasContext native seal")
                        }
                    }
                }
                Current::Error { id }
            }
        };
        self.restore_current(owner, info.generation, id, current);
    }

    pub fn destroy_texture(&self, texture_id: id::TextureId) -> bool {
        self.finish_texture(texture_id, true)
    }

    pub fn drop_texture(&self, texture_id: id::TextureId) -> bool {
        self.finish_texture(texture_id, false)
    }

    fn finish_texture(&self, texture_id: id::TextureId, report_error: bool) -> bool {
        let owner = {
            let configs = self.configs.borrow();
            let entry = configs
                .owners
                .iter()
                .find(|(_, config)| config.current.id() == Some(texture_id));
            match entry {
                Some((_, config))
                    if matches!(
                        config.current,
                        Current::InProgress { .. } | Current::PublishingBlank
                    ) =>
                {
                    self.reject_protocol();
                    return false;
                }
                Some((owner, _)) => Some(*owner),
                None => None,
            }
        };
        if let Some(owner) = owner {
            self.seal_current(owner, report_error);
        }
        true
    }

    pub fn destroy_device(&self, device_id: id::DeviceId) -> bool {
        let owners = {
            let configs = self.configs.borrow();
            if configs.owners.values().any(|config| {
                config.info.device_id == device_id
                    && matches!(
                        config.current,
                        Current::InProgress { .. } | Current::PublishingBlank
                    )
            }) {
                self.reject_protocol();
                return false;
            }
            configs
                .owners
                .iter()
                .filter_map(|(owner, config)| {
                    (config.info.device_id == device_id).then_some(*owner)
                })
                .collect::<Vec<_>>()
        };
        for owner in owners {
            self.seal_current(owner, true);
        }
        true
    }

    pub fn prepare_present(
        &self,
        owner: u64,
        texture_id: id::TextureId,
        remote_id: u64,
    ) -> Option<NativePublication> {
        {
            let configs = self.configs.borrow();
            let Some(config) = configs.owners.get(&owner) else {
                self.reject_protocol();
                return None;
            };
            if !config.registered
                || config.current.id() != Some(texture_id)
                || remote_id == 0
                || config
                    .last_publication
                    .is_some_and(|(last, _)| remote_id <= last)
                || matches!(config.current, Current::InProgress { .. } | Current::Absent)
            {
                self.reject_protocol();
                return None;
            }
        }
        self.seal_current(owner, true);
        let mut configs = self.configs.borrow_mut();
        let Some(config) = configs.owners.get_mut(&owner) else {
            return None;
        };
        if config.current.id() != Some(texture_id) {
            self.reject_protocol();
            return None;
        }
        let current =
            std::mem::replace(&mut config.current, Current::InProgress { id: texture_id });
        let (frame, submission_index) = match current {
            Current::Sealed { frame, .. } => {
                let index = frame.submission_index();
                (Some(frame), index)
            }
            Current::Error { .. } => (None, 0),
            other => {
                config.current = other;
                self.reject_protocol();
                return None;
            }
        };
        Some(NativePublication {
            generation: config.info.generation,
            texture_id: Some(texture_id),
            remote_id,
            frame,
            submission_index,
        })
    }

    pub fn prepare_blank(&self, owner: u64, remote_id: u64) -> Option<NativePublication> {
        let mut configs = self.configs.borrow_mut();
        let Some(config) = configs.owners.get_mut(&owner) else {
            self.reject_protocol();
            return None;
        };
        if !config.registered
            || remote_id == 0
            || config.last_publication.is_some()
            || !matches!(config.current, Current::Absent)
        {
            self.reject_protocol();
            return None;
        }
        config.current = Current::PublishingBlank;
        Some(NativePublication {
            generation: config.info.generation,
            texture_id: None,
            remote_id,
            frame: None,
            submission_index: 0,
        })
    }

    pub fn presented(&self, owner: u64, publication: NativePublication, admitted: bool) {
        if !admitted {
            self.reject_protocol();
        }
        let mut configs = self.configs.borrow_mut();
        if let Some(config) = configs.owners.get_mut(&owner).filter(|config| {
            config.info.generation == publication.generation
                && match publication.texture_id {
                    Some(expected) => {
                        matches!(config.current, Current::InProgress { id } if id == expected)
                    }
                    None => matches!(config.current, Current::PublishingBlank),
                }
        }) {
            if admitted {
                config.current = Current::Absent;
                config.last_publication =
                    Some((publication.remote_id, publication.frame.is_some()));
            } else {
                config.current = publication
                    .texture_id
                    .map_or(Current::Absent, |id| Current::Error { id });
            }
        }
    }

    pub fn configuration_bgra(&self, owner: u64) -> Option<bool> {
        let configs = self.configs.borrow();
        match configs.owners.get(&owner)?.info.descriptor.format {
            wgt::TextureFormat::Rgba8Unorm => Some(false),
            wgt::TextureFormat::Bgra8Unorm => Some(true),
            _ => None,
        }
    }

    pub fn snapshot_kind(&self, owner: u64) -> NativeCanvasSnapshotKind {
        if self.protocol_error() {
            return NativeCanvasSnapshotKind::Unavailable;
        }
        let (device, current, presented_image) = {
            let configs = self.configs.borrow();
            let Some(config) = configs
                .owners
                .get(&owner)
                .filter(|config| config.registered)
            else {
                return NativeCanvasSnapshotKind::Unavailable;
            };
            let presented_image = config.last_publication.is_some_and(|(_, image)| image);
            let current = match config.current {
                Current::Absent if presented_image => NativeCanvasSnapshotKind::Published,
                Current::Absent => NativeCanvasSnapshotKind::Blank,
                Current::Writing { .. } => NativeCanvasSnapshotKind::Writing,
                Current::Sealed { .. } => NativeCanvasSnapshotKind::Sealed,
                Current::Error { .. } => NativeCanvasSnapshotKind::Error,
                Current::InProgress { .. } | Current::PublishingBlank => {
                    NativeCanvasSnapshotKind::Unavailable
                }
            };
            (config.device.clone(), current, presented_image)
        };
        if !device.upgrade().is_some_and(|device| device.is_valid()) {
            return if presented_image {
                NativeCanvasSnapshotKind::Published
            } else {
                NativeCanvasSnapshotKind::Blank
            };
        }
        current
    }

    pub fn snapshot_frame(&self, owner: u64) -> Option<Arc<SealedCanvasFrame>> {
        let (device, frame) = {
            let configs = self.configs.borrow();
            let config = configs.owners.get(&owner)?;
            match &config.current {
                Current::Sealed { frame, .. } => (config.device.clone(), frame.clone()),
                _ => return None,
            }
        };
        device
            .upgrade()
            .filter(|device| device.is_valid())
            .map(|_| frame)
    }

    pub fn create_snapshot_buffer(
        &self,
        global: &CoreGlobal,
        owner: u64,
        buffer_id: id::BufferId,
        size: u64,
    ) -> bool {
        let endpoints = {
            let configs = self.configs.borrow();
            let Some(config) = configs.owners.get(&owner).filter(|config| {
                config.registered && matches!(config.current, Current::Writing { .. })
            }) else {
                return false;
            };
            (
                config.device.clone(),
                config.queue.clone(),
                config.info.generation,
            )
        };
        let (device, queue, generation) = endpoints;
        let (Some(device), Some(queue)) = (device.upgrade(), queue.upgrade()) else {
            return false;
        };
        let Some(nonzero_size) = wgt::BufferSize::new(size) else {
            return false;
        };
        if !device.is_valid()
            || size == 0
            || size > device.limits().max_buffer_size
            || !size.is_multiple_of(wgt::COPY_BUFFER_ALIGNMENT)
            || !global.canvas_buffer_id_available(buffer_id)
        {
            return false;
        }
        device.push_error_scope(wgt::error::ErrorFilter::Validation);
        device.push_error_scope(wgt::error::ErrorFilter::OutOfMemory);
        let buffer = device.create_buffer(&wgc::resource::BufferDescriptor {
            label: None,
            size,
            usage: wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let oom = device.pop_error_scope();
        let invalid = device.pop_error_scope();
        if !matches!(oom, Ok(None))
            || !matches!(invalid, Ok(None))
            || queue
                .validate_write_buffer(buffer.clone(), 0, nonzero_size)
                .is_err()
        {
            return false;
        }
        let current = {
            let configs = self.configs.borrow();
            configs.owners.get(&owner).is_some_and(|config| {
                config.info.generation == generation
                    && config.registered
                    && matches!(config.current, Current::Writing { .. })
            })
        };
        current && global.register_canvas_buffer(buffer_id, buffer)
    }

    pub fn copy_current_to_buffer(
        &self,
        global: &CoreGlobal,
        owner: u64,
        buffer_id: id::BufferId,
        stride: u32,
    ) -> SubmissionIndex {
        let Some((info, id, canvas, queue)) = self.take_writing(owner) else {
            return 0;
        };
        let device = canvas.texture().device().clone();
        let result = match (queue, global.resolve_canvas_buffer(buffer_id)) {
            (Some(queue), Some(buffer)) => unsafe {
                queue.copy_shared_canvas_to_buffer(
                    &canvas,
                    &buffer,
                    wgt::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(stride),
                        rows_per_image: None,
                    },
                )
            }
            .map_err(|error| {
                device.handle_error(error, None, "GPUCanvasContext snapshot");
            }),
            _ => {
                validation(
                    &device,
                    "Native snapshot buffer or queue is unavailable",
                    "GPUCanvasContext snapshot",
                );
                Err(())
            }
        };
        self.restore_current(owner, info.generation, id, Current::Writing { id, canvas });
        result.unwrap_or(0)
    }

    pub fn drop_configuration(&self, owner: u64) {
        self.seal_current(owner, false);
        let discarded = self.configs.borrow_mut().owners.remove(&owner);
        if let Some(Configuration {
            current: Current::Writing { canvas, .. },
            ..
        }) = discarded
        {
            canvas.texture().destroy();
        }
    }
}
