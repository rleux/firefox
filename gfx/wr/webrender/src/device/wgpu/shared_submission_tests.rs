/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{
    self, SharedDeviceOptions, NativeRetirement, Options, Texture, TextureFilter,
};
use std::{
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

static ACCEPTED: AtomicUsize = AtomicUsize::new(0);
static DROPPED: AtomicUsize = AtomicUsize::new(0);
static RECOVERED: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct Faults {
    submit: AtomicBool,
    native_submit_lost: AtomicBool,
    native_receipt_lost: AtomicBool,
    receipt: AtomicBool,
    drain: AtomicBool,
    hold_poll: AtomicBool,
    native: Mutex<Option<Arc<dyn hal::shared::SharedSubmissionReceipt>>>,
    wait: Mutex<Option<hal::external_sync::SemaphoreValue>>,
}
struct Context {
    inner: Arc<dyn hal::shared::SharedDeviceContext>,
    faults: Arc<Faults>,
}
impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WRFaultContext")
    }
}
struct Receipt {
    inner: Arc<dyn hal::shared::SharedSubmissionReceipt>,
    faults: Arc<Faults>,
}
impl fmt::Debug for Receipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WRFaultReceipt")
    }
}
impl hal::shared::SharedSubmissionReceipt for Receipt {
    fn poll(&self) -> Result<bool, hal::DeviceError> {
        if self.faults.native_receipt_lost.load(Ordering::SeqCst) {
            return Err(hal::DeviceError::Lost);
        }
        if self.faults.receipt.load(Ordering::SeqCst) {
            return Err(hal::DeviceError::OutOfMemory);
        }
        Ok(self.inner.poll()? && !self.faults.hold_poll.load(Ordering::SeqCst))
    }
    fn wait(&self, timeout: Option<Duration>) -> Result<bool, hal::DeviceError> {
        if self.faults.native_receipt_lost.load(Ordering::SeqCst) {
            return Err(hal::DeviceError::Lost);
        }
        if self.faults.receipt.load(Ordering::SeqCst) {
            return Err(hal::DeviceError::OutOfMemory);
        }
        self.inner.wait(timeout)
    }
}
impl hal::shared::SharedDeviceContext for Context {
    fn identity(&self) -> hal::shared::SharedDeviceIdentity {
        self.inner.identity()
    }
    fn clone_instance(&self) -> (wgt::Backend, Box<dyn hal::DynInstance>) {
        self.inner.clone_instance()
    }
    unsafe fn open_compute_device(
        self: Arc<Self>,
        adapter: &dyn hal::DynAdapter,
        features: wgt::Features,
        limits: &wgt::Limits,
        hints: &wgt::MemoryHints,
    ) -> Result<hal::DynOpenDevice, hal::shared::SharedDeviceError> {
        unsafe {
            self.inner
                .clone()
                .open_compute_device(adapter, features, limits, hints)
        }
    }
    unsafe fn submit_ordered(
        &self,
        queue: &dyn hal::DynQueue,
        segments: &[hal::shared::SharedSubmitSegment<'_>],
        terminal: (&dyn hal::DynFence, hal::FenceValue),
    ) -> Result<Box<dyn hal::shared::SharedSubmissionReceipt>, hal::shared::SharedSubmitError> {
        unsafe { self.inner.submit_ordered(queue, segments, terminal) }
    }
    unsafe fn submit_graphics(
        &self,
        queue: &dyn hal::DynQueue,
        buffers: &[&dyn hal::DynCommandBuffer],
        surfaces: &[&dyn hal::DynSurfaceTexture],
        terminal: (&dyn hal::DynFence, hal::FenceValue),
        synchronization: &hal::external_sync::SubmissionSync,
    ) -> Result<Box<dyn hal::shared::SharedSubmissionReceipt>, hal::shared::SharedSubmitError> {
        let mut sync = synchronization.clone();
        if let Some(wait) = self.faults.wait.lock().unwrap().as_ref() {
            sync.waits.push(wait.clone());
        }
        let receipt = unsafe {
            self.inner
                .submit_graphics(queue, buffers, surfaces, terminal, &sync)
        }?;
        ACCEPTED.fetch_add(1, Ordering::SeqCst);
        let receipt: Arc<dyn hal::shared::SharedSubmissionReceipt> = Arc::from(receipt);
        *self.faults.native.lock().unwrap() = Some(receipt.clone());
        let wrapped: Box<dyn hal::shared::SharedSubmissionReceipt> = Box::new(Receipt {
            inner: receipt,
            faults: self.faults.clone(),
        });
        if self.faults.native_submit_lost.load(Ordering::SeqCst) {
            Err(hal::shared::SharedSubmitError {
                error: hal::DeviceError::Lost,
                receipt: Some(wrapped),
            })
        } else if self.faults.submit.load(Ordering::SeqCst) {
            Err(hal::shared::SharedSubmitError {
                error: hal::DeviceError::OutOfMemory,
                receipt: Some(wrapped),
            })
        } else {
            Ok(wrapped)
        }
    }
    fn drain(&self) -> Result<(), hal::DeviceError> {
        if self.faults.drain.load(Ordering::SeqCst) {
            Err(hal::DeviceError::OutOfMemory)
        } else {
            self.inner.drain()
        }
    }
}

unsafe extern "C" fn fatal() -> ! {
    let valid = ACCEPTED.load(Ordering::SeqCst) > 0
        && DROPPED.load(Ordering::SeqCst) == 0
        && RECOVERED.load(Ordering::SeqCst) == 0
        && wgpu::tests::ERRORS.load(Ordering::SeqCst) == 0;
    unsafe extern "C" {
        fn _exit(code: i32) -> !;
    }
    unsafe { _exit(if valid { 86 } else { 87 }) }
}

fn owner() -> (
    Rc<Device>,
    Arc<Faults>,
    Arc<dyn hal::shared::SharedDeviceContext>,
) {
    wgpu::tests::validation_logging();
    let instance = wgpu_core::instance::Instance::new(
        "WR shared lifetime proof",
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags: wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION,
            backend_options: Default::default(),
            memory_budget_thresholds: wgt::MemoryBudgetThresholds {
                for_resource_creation: Some(95),
                for_device_loss: Some(99),
            },
            display: None,
        },
        None,
    );
    let mut adapters = unsafe {
        instance
            .raw(wgt::Backend::Vulkan)
            .unwrap()
            .enumerate_adapters(None)
    };
    let index = wgpu::select_adapter(
        adapters
            .iter()
            .map(|a| (a.info.name.as_str(), a.info.device_type)),
        None,
    )
    .unwrap();
    let exposed = adapters.swap_remove(index);
    let pair = unsafe {
        hal::shared::open_shared_device(
            &*exposed.adapter,
            Device::shared_features(exposed.features),
            &exposed.capabilities.limits,
            &wgt::MemoryHints::default(),
        )
    }
    .unwrap();
    let inner = pair.context;
    let (_, imported) = inner.clone_instance();
    let faults = Arc::new(Faults::default());
    let context: Arc<dyn hal::shared::SharedDeviceContext> = Arc::new(Context {
        inner: inner.clone(),
        faults: faults.clone(),
    });
    let owner = Rc::new(
        unsafe {
            Device::from_shared_endpoint(
                Arc::new(pair.graphics),
                imported,
                exposed,
                SharedDeviceOptions {
                    context,
                    recover: Rc::new(|retirement| {
                        assert_eq!(retirement, NativeRetirement::Confirmed);
                        RECOVERED.fetch_add(1, Ordering::SeqCst);
                    }),
                    fatal_process: fatal,
                },
            )
        }
        .unwrap(),
    );
    assert!(
        hal::renderer::graphics_capabilities(
            &*owner.instance,
            &*owner.adapter,
            &*owner.open.device
        )
        .unwrap()
        .validation_enabled
    );
    (owner, faults, inner)
}

struct Probe;
impl Drop for Probe {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

fn record(owner: &Rc<Device>, commands: &mut Recording<'_>) -> std::rc::Weak<Probe> {
    let probe = Rc::new(Probe);
    let weak = Rc::downgrade(&probe);
    commands.keep(&probe);
    let (raw, _) = unsafe {
        owner.open.device.create_buffer(&hal::BufferDescriptor {
            label: None,
            size: 4,
            usage: wgt::BufferUses::COPY_DST,
            memory_flags: hal::MemoryFlags::empty(),
        })
    }
    .unwrap();
    let buffer = Rc::new(Owned::new(owner, raw, <dyn hal::DynDevice>::destroy_buffer));
    commands.keep(&buffer);
    unsafe {
        commands.encoder().transition_buffers(&[hal::BufferBarrier {
            buffer: &**buffer,
            usage: hal::StateTransition {
                from: wgt::BufferUses::empty(),
                to: wgt::BufferUses::COPY_DST,
            },
        }]);
        commands.encoder().clear_buffer(&**buffer, 0..4);
    }
    weak
}

#[test]
#[ignore = "Requires shared Vulkan and Khronos validation"]
fn shared_receipt_retains_ordinary_wr_resources_until_ready() {
    DROPPED.store(0, Ordering::SeqCst);
    RECOVERED.store(0, Ordering::SeqCst);
    let (owner, faults, context) = owner();
    faults.hold_poll.store(true, Ordering::SeqCst);
    let mut submission = Submission::new(&owner).unwrap();
    let weak = record(&owner, &mut submission.recording().unwrap());
    submission.submit().unwrap();
    assert!(faults
        .native
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .wait(None)
        .unwrap());
    assert!(!submission.poll().unwrap());
    assert!(weak.upgrade().is_some());
    assert_eq!(DROPPED.load(Ordering::SeqCst), 0);
    faults.hold_poll.store(false, Ordering::SeqCst);
    assert!(submission.poll().unwrap());
    assert!(weak.upgrade().is_none());
    assert_eq!(DROPPED.load(Ordering::SeqCst), 1);
    drop(submission);
    context.drain().unwrap();
    drop((owner, faults, context));
    assert_eq!(RECOVERED.load(Ordering::SeqCst), 0);
    assert_eq!(wgpu::tests::ERRORS.load(Ordering::SeqCst), 0);
}

#[test]
#[ignore = "Requires shared Vulkan and Khronos validation"]
fn shared_confirmed_failure_notifies_once_and_retains_until_drop() {
    DROPPED.store(0, Ordering::SeqCst);
    RECOVERED.store(0, Ordering::SeqCst);
    let (owner, faults, context) = owner();
    let mut submission = Submission::new(&owner).unwrap();
    let weak = record(&owner, &mut submission.recording().unwrap());
    faults.submit.store(true, Ordering::SeqCst);
    assert!(submission.submit().is_err());
    assert!(owner.is_lost());
    assert_eq!(RECOVERED.load(Ordering::SeqCst), 1);
    assert!(weak.upgrade().is_some());
    drop(submission);
    assert!(weak.upgrade().is_none());
    assert_eq!(DROPPED.load(Ordering::SeqCst), 1);
    context.drain().unwrap();
    drop((owner, faults, context));
    assert_eq!(RECOVERED.load(Ordering::SeqCst), 1);
    assert_eq!(wgpu::tests::ERRORS.load(Ordering::SeqCst), 0);
}

fn child(mode: &str) -> ! {
    ACCEPTED.store(0, Ordering::SeqCst);
    DROPPED.store(0, Ordering::SeqCst);
    RECOVERED.store(0, Ordering::SeqCst);
    let (owner, faults, _context) = owner();
    if mode.starts_with("native-") {
        let mut submission = Submission::new(&owner).unwrap();
        record(&owner, &mut submission.recording().unwrap());
        faults.native_submit_lost.store(mode == "native-submit", Ordering::SeqCst);
        submission.submit().unwrap();
        faults.native_receipt_lost.store(true, Ordering::SeqCst);
        if mode == "native-drop" {
            drop(submission);
        } else if mode == "native-poll" {
            let _ = submission.poll();
        } else {
            let _ = submission.wait(None);
        }
        std::process::exit(87);
    }
    let target = if mode == "readback" {
        let texture = Texture::new(
            &owner,
            1,
            1,
            wgt::TextureFormat::Rgba8Unorm,
            TextureFilter::Nearest,
            false,
        )
        .unwrap();
        let queue = SubmissionQueue::new(&Rc::new(BufferPool::new(&owner)), 2).unwrap();
        texture
            .upload(
                &queue,
                api::units::DeviceIntRect::from_size(api::units::DeviceIntSize::new(1, 1)),
                &[1, 2, 3, 255],
                None,
                0,
                None,
            )
            .unwrap();
        queue.wait().unwrap();
        Some(texture)
    } else {
        None
    };
    let producer = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Default::default()
        })
        .unwrap(),
    );
    let timeline = hal::external_sync::create_timeline(&*producer.open.device, true).unwrap();
    let handle = hal::external_sync::export_timeline(&*timeline).unwrap();
    let imported = hal::external_sync::import_timeline(&*owner.open.device, &handle).unwrap();
    *faults.wait.lock().unwrap() = Some(hal::external_sync::SemaphoreValue {
        semaphore: imported,
        value: Some(1),
    });
    ACCEPTED.store(0, Ordering::SeqCst);
    faults.drain.store(true, Ordering::SeqCst);
    if mode == "queue" {
        let queue = SubmissionQueue::new(&Rc::new(BufferPool::new(&owner)), 2).unwrap();
        record(&owner, &mut queue.recording().unwrap());
        faults.submit.store(true, Ordering::SeqCst);
        let _ = queue.submit();
    } else if let Some(texture) = target {
        faults.submit.store(true, Ordering::SeqCst);
        let _ = texture.readback(api::units::DeviceIntRect::from_size(
            api::units::DeviceIntSize::new(1, 1),
        ));
    } else {
        let mut submission = Submission::new(&owner).unwrap();
        record(&owner, &mut submission.recording().unwrap());
        if mode == "submit" {
            faults.submit.store(true, Ordering::SeqCst);
        }
        let _ = submission.submit();
        faults.receipt.store(true, Ordering::SeqCst);
        if mode == "wait" {
            let _ = submission.wait(None);
        } else {
            drop(submission);
        }
    }
    unsafe extern "C" {
        fn _exit(code: i32) -> !;
    }
    unsafe { _exit(87) }
}

#[test]
#[ignore = "Requires shared Vulkan semaphore FD support and Khronos validation"]
fn shared_unconfirmed_cleanup_terminates_before_releasing_owners() {
    const CHILD: &str = "WR_SHARED_RETIREMENT_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        child(&mode.to_string_lossy());
    }
    for mode in ["submit", "wait", "drop", "queue", "readback", "native-submit", "native-poll", "native-wait", "native-drop"] {
        let mut process = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "device::wgpu::submission::shared_tests::shared_unconfirmed_cleanup_terminates_before_releasing_owners", "--nocapture"])
            .env(CHILD, mode).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let timed_out = loop {
            if process.try_wait().unwrap().is_some() {
                break false;
            }
            if std::time::Instant::now() >= deadline {
                process.kill().unwrap();
                break true;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let output = process.wait_with_output().unwrap();
        assert!(!timed_out, "{mode} did not terminate");
        assert_eq!(
            output.status.code(),
            Some(86),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
