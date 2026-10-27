/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock, Weak,
};
use wgh::{
    shared::{NativeRetirement, SharedDeviceContext, SharedDeviceIdentity},
    DynOpenDevice,
};
pub use wgc::device::queue::FinalizedCanvas;
use wgpu_core_remote::{global::Global, id};
use wgpu_core_remote_types::{Label, RequestDeviceError};

pub struct ProcessOwner {
    context: Arc<dyn SharedDeviceContext>,
    graphics: Arc<DynOpenDevice>,
    factory: Arc<dyn wgh::GraphicsRecorderFactory>,
    flags: wgt::InstanceFlags,
    native_features: wgt::Features,
    validation_enabled: bool,
    clients_closed: AtomicBool,
    native_failed: AtomicBool,
    fatal: unsafe extern "C" fn() -> !,
}
impl std::fmt::Debug for ProcessOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedProcessOwner")
            .field("identity", &self.identity())
            .field("native_features", &self.native_features)
            .finish_non_exhaustive()
    }
}
static STARTUP: OnceLock<Result<Weak<ProcessOwner>, String>> = OnceLock::new();

pub fn available_owner(is_gpu_process: bool) -> Option<Arc<ProcessOwner>> {
    if !is_gpu_process {
        return None;
    }
    let owner = STARTUP.get()?.as_ref().ok()?.upgrade()?;
    owner.is_ready().then_some(owner)
}

/// Publish a single weak process lookup; the caller retains the strong owner.
/// # Safety
/// The factory/fatal hook and native-owner lifetime requirements of create apply.
pub unsafe fn bootstrap(
    factory: Arc<dyn wgh::GraphicsRecorderFactory>,
    fatal: unsafe extern "C" fn() -> !,
) -> Result<Arc<ProcessOwner>, String> {
    let mut created = None;
    let startup = STARTUP.get_or_init(|| {
        let mut descriptor = crate::server::instance_descriptor();
        if static_prefs::pref!("dom.webgpu.wgpu-backend").is_empty() {
            descriptor.backends = wgt::Backends::VULKAN;
        }
        let require_hardware = static_prefs::pref!("dom.webgpu.testing.assert-hardware-adapter");
        match unsafe { ProcessOwner::create(descriptor, factory, fatal, require_hardware) } {
            Ok(owner) => {
                created = Some(owner.clone());
                Ok(Arc::downgrade(&owner))
            }
            Err(error) => Err(error),
        }
    });
    let owner = created
        .or_else(|| startup.as_ref().ok().and_then(Weak::upgrade))
        .ok_or_else(|| {
            startup
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "Unified process owner has expired".into())
        })?;
    owner.check_ready()?;
    Ok(owner)
}

impl ProcessOwner {
    /// # Safety
    /// Use only in an isolated graphics process. Fatal must terminate it without
    /// returning/unwinding; all accepted work must retain its native resources.
    pub unsafe fn create(
        descriptor: wgt::InstanceDescriptor,
        factory: Arc<dyn wgh::GraphicsRecorderFactory>,
        fatal: unsafe extern "C" fn() -> !,
        require_hardware: bool,
    ) -> Result<Arc<Self>, String> {
        if !descriptor.backends.contains(wgt::Backends::VULKAN) {
            return Err("Unified WebGPU requires the Vulkan backend".into());
        }
        if descriptor.display.is_some() {
            return Err("Unified native bootstrap must be window-independent".into());
        }
        let flags = descriptor.flags;
        let endpoints = unsafe {
            wgh::renderer::bootstrap_shared_device(
                &wgh::InstanceDescriptor {
                    name: "Unified WebRender/WebGPU",
                    flags,
                    memory_budget_thresholds: descriptor.memory_budget_thresholds,
                    backend_options: descriptor.backend_options,
                    telemetry: Some(crate::telemetry::build_telemetry_struct()),
                    display: None,
                },
                require_hardware,
            )
        }?;
        let context = endpoints.context;
        let graphics = Arc::new(endpoints.graphics);
        drop(endpoints.compute);
        let (_, instance) = context.clone_instance();
        let mut adapters = unsafe { instance.enumerate_adapters(None) };
        if adapters.len() != 1 {
            return Err("Unified owner must expose exactly its selected adapter".into());
        }
        let adapter = adapters.pop().unwrap();
        let capabilities =
            wgh::renderer::graphics_capabilities(&*instance, &*adapter.adapter, &*graphics.device)
                .map_err(|error| error.to_string())?;
        Ok(Arc::new(Self {
            context,
            graphics,
            factory,
            flags,
            native_features: capabilities.features,
            validation_enabled: capabilities.validation_enabled,
            clients_closed: AtomicBool::new(false),
            native_failed: AtomicBool::new(false),
            fatal,
        }))
    }
    fn check_ready(&self) -> Result<(), String> {
        if self.is_ready() {
            Ok(())
        } else {
            Err("Unified native owner no longer admits clients".into())
        }
    }
    pub fn is_ready(&self) -> bool {
        !self.clients_closed.load(Ordering::Acquire)
            && !self.native_failed.load(Ordering::Acquire)
            && !self.context.is_closed()
    }
    pub fn begin_shutdown(&self) {
        self.clients_closed.store(true, Ordering::Release);
    }
    pub fn identity(&self) -> SharedDeviceIdentity {
        self.context.identity()
    }
    pub fn native_features(&self) -> wgt::Features {
        self.native_features
    }
    pub fn flags(&self) -> wgt::InstanceFlags {
        self.flags
    }
    pub fn validation_enabled(&self) -> bool {
        self.validation_enabled
    }
    pub fn graphics(&self) -> Arc<DynOpenDevice> {
        self.graphics.clone()
    }
    pub fn context(&self) -> Arc<dyn SharedDeviceContext> {
        self.context.clone()
    }
    pub fn fatal_process(&self) -> unsafe extern "C" fn() -> ! {
        self.fatal
    }
    pub fn clone_graphics_instance(
        &self,
    ) -> Result<(Box<dyn wgh::DynInstance>, wgh::DynExposedAdapter), String> {
        self.check_ready()?;
        let (_, instance) = self.context.clone_instance();
        let mut adapters = unsafe { instance.enumerate_adapters(None) };
        if adapters.len() != 1 {
            return Err("Unified owner adapter is unavailable".into());
        }
        Ok((instance, adapters.pop().unwrap()))
    }
    pub fn new_actor_global(&self) -> Result<Global, String> {
        self.check_ready()?;
        let (backend, instance) = self.context.clone_instance();
        let instance = unsafe {
            wgc::instance::Instance::from_hal_instance_dynamic(
                "Unified WebGPU actor".into(),
                backend,
                instance,
                self.flags,
                None,
            )
        };
        Ok(Global::from_instance(instance))
    }
    pub fn create_device<'a>(
        self: &Arc<Self>,
        global: &Global,
        adapter_id: id::AdapterId,
        desc: &mut wgt::DeviceDescriptor<Label<'a>>,
        device_id: id::DeviceId,
        queue_id: id::QueueId,
    ) -> Result<(), RequestDeviceError> {
        self.check_ready().map_err(RequestDeviceError::Other)?;
        global.adapter_validate_device_descriptor(adapter_id, desc)?;
        let adapter = global.resolve_adapter_id(adapter_id);
        let open = unsafe {
            self.context.clone().open_compute_device(
                adapter.as_hal_dynamic(),
                desc.required_features,
                &desc.required_limits,
                &desc.memory_hints,
            )
        }
        .map_err(|error| {
            if matches!(
                &error,
                wgh::shared::SharedDeviceError::Device(wgh::DeviceError::Lost)
            ) {
                self.native_failed.store(true, Ordering::Release);
                unsafe { (self.fatal)() }
            }
            RequestDeviceError::Other(error.to_string())
        })?;
        let provider = Arc::new(Provider {
            owner: self.clone(),
            receipts: std::sync::Mutex::new(Vec::new()),
        });
        unsafe {
            global.create_device_from_hal_with_graphics(
                adapter_id,
                open,
                desc,
                device_id,
                queue_id,
                Some(provider),
            )
        }?;
        Ok(())
    }
    pub fn recover_native_failure(&self, retirement: NativeRetirement) {
        if retirement == NativeRetirement::Unconfirmed
            || self.native_failed.load(Ordering::Acquire)
            || self.context.is_closed()
        {
            self.begin_shutdown();
            unsafe { (self.fatal)() }
        }
    }
}
#[derive(Debug)]
struct Provider {
    owner: Arc<ProcessOwner>,
    receipts: std::sync::Mutex<Vec<Weak<AcceptedReceipt>>>,
}

#[derive(Debug)]
struct AcceptedReceipt {
    native: Box<dyn wgh::shared::SharedSubmissionReceipt>,
    owner: Weak<ProcessOwner>,
}
impl AcceptedReceipt {
    fn observe<T>(&self, result: Result<T, wgh::DeviceError>) -> Result<T, wgh::DeviceError> {
        if matches!(&result, Err(wgh::DeviceError::Lost)) {
            if let Some(owner) = self.owner.upgrade() {
                owner.native_failed.store(true, Ordering::Release);
            }
        }
        result
    }
    fn wait(&self, timeout: Option<std::time::Duration>) -> Result<bool, wgh::DeviceError> {
        self.observe(self.native.wait(timeout))
    }
}
#[derive(Debug)]
struct Receipt(Arc<AcceptedReceipt>);
impl wgh::shared::SharedSubmissionReceipt for Receipt {
    fn poll(&self) -> Result<bool, wgh::DeviceError> {
        self.0.observe(self.0.native.poll())
    }
    fn wait(&self, timeout: Option<std::time::Duration>) -> Result<bool, wgh::DeviceError> {
        self.0.wait(timeout)
    }
}
impl Provider {
    fn retain(
        &self,
        native: Box<dyn wgh::shared::SharedSubmissionReceipt>,
    ) -> Box<dyn wgh::shared::SharedSubmissionReceipt> {
        let receipt = Arc::new(AcceptedReceipt {
            native,
            owner: Arc::downgrade(&self.owner),
        });
        let mut receipts = self
            .receipts
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        receipts.retain(|receipt| receipt.strong_count() != 0);
        receipts.push(Arc::downgrade(&receipt));
        Box::new(Receipt(receipt))
    }
}
impl wgc::graphics::GraphicsRecordingProvider for Provider {
    fn submission(self: Arc<Self>) -> Option<Arc<dyn wgc::graphics::GraphicsSubmissionProvider>> {
        Some(self)
    }
    unsafe fn create_encoder(
        &self,
        _device: &dyn wgh::DynDevice,
        native: Box<dyn wgh::DynCommandEncoder>,
    ) -> Result<Box<dyn wgh::DynCommandEncoder>, wgh::DeviceError> {
        if self.owner.native_failed.load(Ordering::Acquire) || self.owner.context.is_closed() {
            return Err(wgh::DeviceError::Lost);
        }
        let result = unsafe { self.owner.factory.create(self.owner.graphics.clone()) };
        if matches!(&result, Err(wgh::DeviceError::Lost)) {
            self.owner.native_failed.store(true, Ordering::Release);
            unsafe { (self.owner.fatal)() }
        }
        result.map(|graphics| {
            Box::new(wgpu_graphics::RoutedEncoder::new(native, graphics))
                as Box<dyn wgh::DynCommandEncoder>
        })
    }
}
impl wgc::graphics::GraphicsSubmissionProvider for Provider {
    fn seal_encoder(
        &self,
        encoder: &mut dyn wgh::DynCommandEncoder,
    ) -> Result<(), wgh::DeviceError> {
        encoder
            .as_any_mut()
            .downcast_mut::<wgpu_graphics::RoutedEncoder>()
            .ok_or(wgh::DeviceError::Unexpected)?
            .seal_graphics()
    }

    fn submission_parts<'a>(
        &self,
        encoder: &'a dyn wgh::DynCommandEncoder,
        buffers: &'a [Box<dyn wgh::DynCommandBuffer>],
        queue: &'a dyn wgh::DynQueue,
    ) -> Result<Vec<wgc::graphics::GraphicsSubmitPart<'a>>, wgh::DeviceError> {
        encoder
            .as_any()
            .downcast_ref::<wgpu_graphics::RoutedEncoder>()
            .ok_or(wgh::DeviceError::Unexpected)?
            .submit_parts(buffers, queue)
            .map(|parts| {
                parts
                    .into_iter()
                    .map(|part| wgc::graphics::GraphicsSubmitPart {
                        lane: part.lane,
                        queue: part.queue,
                        buffer: part.buffer,
                    })
                    .collect()
            })
    }

    fn graphics_queue(&self) -> Option<&dyn wgh::DynQueue> {
        Some(&*self.owner.graphics.queue)
    }
    unsafe fn submit(
        &self,
        logical_queue: &dyn wgh::DynQueue,
        segments: &[wgh::shared::SharedSubmitSegment<'_>],
        terminal: (&dyn wgh::DynFence, wgh::FenceValue),
    ) -> Result<Box<dyn wgh::shared::SharedSubmissionReceipt>, wgh::shared::SharedSubmitError> {
        if self.owner.native_failed.load(Ordering::Acquire) {
            return Err(wgh::shared::SharedSubmitError {
                error: wgh::DeviceError::Lost,
                receipt: None,
            });
        }
        match unsafe {
            self.owner
                .context
                .submit_ordered(logical_queue, segments, terminal)
        } {
            Ok(receipt) => Ok(self.retain(receipt)),
            Err(mut error) => {
                if matches!(&error.error, wgh::DeviceError::Lost) {
                    self.owner.native_failed.store(true, Ordering::Release);
                    unsafe { (self.owner.fatal)() }
                }
                if let Some(receipt) = error.receipt.take() {
                    error.receipt = Some(self.retain(receipt));
                }
                Err(error)
            }
        }
    }
    fn drain(&self) -> Result<(), wgh::DeviceError> {
        if self.owner.context.is_closed() || self.owner.native_failed.load(Ordering::Acquire) {
            return self.owner.context.drain();
        }
        let receipts = {
            let mut entries = self
                .receipts
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let receipts = entries.iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
            entries.retain(|receipt| receipt.strong_count() != 0);
            receipts
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        for receipt in receipts {
            match receipt.wait(Some(
                deadline.saturating_duration_since(std::time::Instant::now()),
            )) {
                Ok(true) => {}
                Ok(false) => return Err(wgh::DeviceError::Unexpected),
                Err(wgh::DeviceError::Lost) => return self.owner.context.drain(),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    fn request_recovery(&self, retirement: NativeRetirement) {
        self.owner.recover_native_failure(retirement);
    }
    fn fatal_process_termination(&self) -> unsafe extern "C" fn() -> ! {
        self.owner.fatal
    }
}
