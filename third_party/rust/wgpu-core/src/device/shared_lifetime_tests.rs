use super::*;
use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    sync::Weak,
    vec,
};
use core::{
    any::Any,
    fmt,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    time::Duration,
};
use std::sync::Mutex as StdMutex;

use crate::device::Device;
use crate::resource::RawResourceAccess;
use crate::graphics::{GraphicsRecordingProvider, GraphicsSubmissionProvider, NativeRetirement};

const SUCCESS: usize = 0;
const REJECT: usize = 1;
const PARTIAL: usize = 2;

#[derive(Default)]
struct Driver {
    mode: AtomicUsize,
    clear_error: AtomicBool,
    end_error: AtomicBool,
    hide_graphics_queue: AtomicBool,
    segments: StdMutex<Vec<(hal::shared::SharedQueueLane, bool, bool)>>,
    ready: AtomicU64,
    poll_error: AtomicBool,
    wait_error: AtomicBool,
    drain_error: AtomicBool,
    delay_ms: AtomicU64,
    submissions: AtomicUsize,
    admissions: AtomicUsize,
    fatal_captures: AtomicUsize,
    executions_dropped: AtomicUsize,
    device: StdMutex<Weak<Device>>,
    recoveries: StdMutex<Vec<NativeRetirement>>,
    waits: StdMutex<Vec<Option<Duration>>>,
    events: StdMutex<Vec<&'static str>>,
    errors: StdMutex<Vec<String>>,
    probes: StdMutex<Vec<std::thread::JoinHandle<()>>>,
    unlocked: AtomicBool,
}

impl Driver {
    fn probe_unlocked(self: &Arc<Self>, event: &'static str) {
        self.events.lock().unwrap().push(event);
        let device = self.device.lock().unwrap().clone();
        if let Some(device) = device.upgrade() {
            let (sender, receiver) = std::sync::mpsc::channel();
            let probe = std::thread::spawn(move || {
                let snatch = device.snatchable_lock.write();
                let indices = device.command_indices.write();
                if let Some(queue) = device.get_queue() {
                    let pending = queue.pending_writes.lock();
                    drop(pending);
                }
                drop(indices);
                drop(snatch);
                let _ = sender.send(());
            });
            let unlocked = receiver.recv_timeout(Duration::from_secs(1)).is_ok();
            self.unlocked.fetch_and(unlocked, Ordering::SeqCst);
            self.probes.lock().unwrap().push(probe);
        }
    }

    fn join_probes(&self) {
        let probes = core::mem::take(&mut *self.probes.lock().unwrap());
        for probe in probes {
            probe.join().unwrap();
        }
    }
}

struct Provider {
    graphics: Arc<hal::DynOpenDevice>,
    driver: Arc<Driver>,
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MockSharedProvider")
    }
}

impl GraphicsRecordingProvider for Provider {
    fn submission(self: Arc<Self>) -> Option<Arc<dyn GraphicsSubmissionProvider>> {
        self.driver.admissions.fetch_add(1, Ordering::SeqCst);
        Some(self)
    }

    unsafe fn create_encoder(
        &self,
        _device: &dyn hal::DynDevice,
        native: Box<dyn hal::DynCommandEncoder>,
    ) -> Result<Box<dyn hal::DynCommandEncoder>, hal::DeviceError> {
        Ok(Box::new(wgpu_graphics::RoutedEncoder::new(
            native,
            Box::new(Recorder {
                owner: self.graphics.clone(),
                driver: self.driver.clone(),
                buffers: Vec::new(),
                open: false,
                pass: false,
            }),
        )))
    }
}

unsafe extern "C" fn terminate_test_process() -> ! {
    std::process::exit(86)
}

impl GraphicsSubmissionProvider for Provider {
    fn seal_encoder(
        &self,
        encoder: &mut dyn hal::DynCommandEncoder,
    ) -> Result<(), hal::DeviceError> {
        encoder
            .as_any_mut()
            .downcast_mut::<wgpu_graphics::RoutedEncoder>()
            .ok_or(hal::DeviceError::Unexpected)?
            .seal_graphics()
    }

    fn submission_parts<'a>(
        &self,
        encoder: &'a dyn hal::DynCommandEncoder,
        buffers: &'a [Box<dyn hal::DynCommandBuffer>],
        queue: &'a dyn hal::DynQueue,
    ) -> Result<Vec<crate::graphics::GraphicsSubmitPart<'a>>, hal::DeviceError> {
        encoder
            .as_any()
            .downcast_ref::<wgpu_graphics::RoutedEncoder>()
            .ok_or(hal::DeviceError::Unexpected)?
            .submit_parts(buffers, queue)
            .map(|parts| {
                parts
                    .into_iter()
                    .map(|part| crate::graphics::GraphicsSubmitPart {
                        lane: part.lane,
                        queue: part.queue,
                        buffer: part.buffer,
                    })
                    .collect()
            })
    }

    fn graphics_queue(&self) -> Option<&dyn hal::DynQueue> {
        if self.driver.hide_graphics_queue.load(Ordering::SeqCst) {
            None
        } else {
            Some(self.graphics.queue.as_ref())
        }
    }

    unsafe fn submit(
        &self,
        queue: &dyn hal::DynQueue,
        segments: &[hal::shared::SharedSubmitSegment<'_>],
        terminal: (&dyn hal::DynFence, hal::FenceValue),
    ) -> Result<Box<dyn hal::shared::SharedSubmissionReceipt>, hal::shared::SharedSubmitError> {
        self.driver.submissions.fetch_add(1, Ordering::SeqCst);
        self.driver
            .segments
            .lock()
            .unwrap()
            .extend(segments.iter().map(|segment| {
                (
                    segment.lane,
                    segment.command_buffers.is_empty(),
                    core::ptr::eq(segment.queue, self.graphics.queue.as_ref()),
                )
            }));
        if self.driver.mode.load(Ordering::SeqCst) == REJECT {
            return Err(hal::shared::SharedSubmitError {
                error: hal::DeviceError::Unexpected,
                receipt: None,
            });
        }
        unsafe { queue.submit(&[], &[], terminal) }.unwrap();
        let receipt: Box<dyn hal::shared::SharedSubmissionReceipt> = Box::new(Receipt {
            driver: self.driver.clone(),
            index: terminal.1,
        });
        if self.driver.mode.load(Ordering::SeqCst) == PARTIAL {
            Err(hal::shared::SharedSubmitError {
                error: hal::DeviceError::OutOfMemory,
                receipt: Some(receipt),
            })
        } else {
            Ok(receipt)
        }
    }

    fn drain(&self) -> Result<(), hal::DeviceError> {
        if self.driver.drain_error.load(Ordering::SeqCst) {
            Err(hal::DeviceError::OutOfMemory)
        } else {
            Ok(())
        }
    }

    fn request_recovery(&self, retirement: NativeRetirement) {
        self.driver.recoveries.lock().unwrap().push(retirement);
        self.driver.probe_unlocked("recovery");
    }

    fn fatal_process_termination(&self) -> unsafe extern "C" fn() -> ! {
        self.driver.fatal_captures.fetch_add(1, Ordering::SeqCst);
        terminate_test_process
    }
}

#[derive(Debug)]
struct BufferToken;
impl hal::DynResource for BufferToken {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
impl hal::DynCommandBuffer for BufferToken {}

struct Recorder {
    owner: Arc<hal::DynOpenDevice>,
    driver: Arc<Driver>,
    buffers: Vec<Box<dyn hal::DynCommandBuffer>>,
    open: bool,
    pass: bool,
}
impl fmt::Debug for Recorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MockGraphicsRecorder")
    }
}
impl hal::GraphicsRecorder for Recorder {
    unsafe fn begin_encoding(&mut self, _label: hal::Label) -> Result<(), hal::DeviceError> {
        assert!(!self.open);
        self.open = true;
        Ok(())
    }
    unsafe fn begin_render_pass(
        &mut self,
        _desc: &hal::RenderPassDescriptor<dyn hal::DynQuerySet, dyn hal::DynTextureView>,
    ) -> Result<(), hal::DeviceError> {
        assert!(self.open && !self.pass);
        if self.driver.clear_error.swap(false, Ordering::SeqCst) {
            return Err(hal::DeviceError::OutOfMemory);
        }
        self.pass = true;
        Ok(())
    }
    unsafe fn record(&mut self, _command: hal::GraphicsCommand<'_>) {
        assert!(self.open && self.pass);
    }
    unsafe fn end_render_pass(&mut self) {
        assert!(self.pass);
        self.pass = false;
    }
    unsafe fn end_encoding(&mut self) -> Result<usize, hal::DeviceError> {
        assert!(self.open && !self.pass);
        if self.driver.end_error.swap(false, Ordering::SeqCst) {
            return Err(hal::DeviceError::OutOfMemory);
        }
        self.open = false;
        let index = self.buffers.len();
        self.buffers.push(Box::new(BufferToken));
        Ok(index)
    }
    fn finish(self: Box<Self>) -> Result<Box<dyn hal::GraphicsExecution>, hal::DeviceError> {
        assert!(!self.open && !self.pass);
        Ok(Box::new(Execution {
            owner: self.owner,
            driver: self.driver,
            buffers: self.buffers,
        }))
    }
}

struct Execution {
    owner: Arc<hal::DynOpenDevice>,
    driver: Arc<Driver>,
    buffers: Vec<Box<dyn hal::DynCommandBuffer>>,
}
impl fmt::Debug for Execution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MockGraphicsExecution")
    }
}
impl hal::GraphicsExecution for Execution {
    fn command_buffers(&self) -> &[Box<dyn hal::DynCommandBuffer>] {
        &self.buffers
    }
    fn queue(&self) -> &dyn hal::DynQueue {
        &*self.owner.queue
    }
}
impl Drop for Execution {
    fn drop(&mut self) {
        self.driver
            .executions_dropped
            .fetch_add(1, Ordering::SeqCst);
    }
}

struct Receipt {
    driver: Arc<Driver>,
    index: u64,
}
impl fmt::Debug for Receipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MockReceipt").field(&self.index).finish()
    }
}
impl hal::shared::SharedSubmissionReceipt for Receipt {
    fn poll(&self) -> Result<bool, hal::DeviceError> {
        if self.driver.poll_error.load(Ordering::SeqCst) {
            return Err(hal::DeviceError::OutOfMemory);
        }
        Ok(self.index <= self.driver.ready.load(Ordering::SeqCst))
    }
    fn wait(&self, timeout: Option<Duration>) -> Result<bool, hal::DeviceError> {
        self.driver.waits.lock().unwrap().push(timeout);
        if self.driver.wait_error.load(Ordering::SeqCst) {
            return Err(hal::DeviceError::OutOfMemory);
        }
        let delay = Duration::from_millis(self.driver.delay_ms.load(Ordering::SeqCst));
        std::thread::sleep(timeout.map_or(delay, |timeout| timeout.min(delay)));
        if timeout.is_some_and(|timeout| timeout < delay) {
            return Ok(false);
        }
        self.driver.ready.fetch_max(self.index, Ordering::SeqCst);
        Ok(true)
    }
}

struct Fixture {
    device: Arc<Device>,
    queue: Arc<crate::device::queue::Queue>,
    driver: Arc<Driver>,
}

impl Fixture {
    fn new() -> Self {
        let instance = crate::instance::Instance::new(
            "shared core lifetime test",
            wgt::InstanceDescriptor {
                backends: wgt::Backends::NOOP,
                flags: wgt::InstanceFlags::empty(),
                memory_budget_thresholds: Default::default(),
                backend_options: wgt::BackendOptions {
                    noop: wgt::NoopBackendOptions::enabled(),
                    ..Default::default()
                },
                display: None,
            },
            None,
        );
        let adapter = instance
            .enumerate_adapters(wgt::Backends::NOOP, false)
            .pop()
            .unwrap();
        let mut desc = crate::device::DeviceDescriptor {
            required_features: wgt::Features::TIMESTAMP_QUERY,
            ..Default::default()
        };
        adapter.validate_device_descriptor(&mut desc).unwrap();
        let open = || {
            unsafe {
                adapter.as_hal_dynamic().open(
                    desc.required_features,
                    &desc.required_limits,
                    &desc.memory_hints,
                )
            }
            .unwrap()
        };
        let driver = Arc::new(Driver {
            mode: AtomicUsize::new(SUCCESS),
            unlocked: AtomicBool::new(true),
            ..Default::default()
        });
        let provider = Arc::new(Provider {
            graphics: Arc::new(open()),
            driver: driver.clone(),
        });
        let (device, queue) = unsafe {
            adapter.create_device_and_queue_from_hal_with_graphics(open(), &desc, Some(provider))
        }
        .unwrap();
        *driver.device.lock().unwrap() = Arc::downgrade(&device);
        let lost = driver.clone();
        device.set_device_lost_closure(Box::new(move |_, _| lost.probe_unlocked("lost")));
        let errors = driver.clone();
        device.on_uncaptured_error(Arc::new(move |error: wgt::error::Error| {
            errors.errors.lock().unwrap().push(error.to_string());
        }));
        Self {
            device,
            queue,
            driver,
        }
    }

    fn recorded(
        &self,
    ) -> (
        Arc<crate::command::CommandBuffer>,
        Arc<Buffer>,
        Arc<Texture>,
        Arc<QuerySet>,
    ) {
        use crate::command;
        let source = self
            .device
            .create_buffer(&crate::resource::BufferDescriptor {
                label: None,
                size: 4,
                usage: wgt::BufferUsages::COPY_SRC | wgt::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        let buffer = self
            .device
            .create_buffer(&crate::resource::BufferDescriptor {
                label: None,
                size: 4,
                usage: wgt::BufferUsages::MAP_READ | wgt::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        let texture = self
            .device
            .create_texture(&crate::resource::TextureDescriptor {
                label: None,
                size: wgt::Extent3d {
                    width: 2,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgt::TextureDimension::D2,
                format: wgt::TextureFormat::Rgba8Unorm,
                usage: wgt::TextureUsages::RENDER_ATTACHMENT,
                view_formats: Vec::new(),
            });
        let query = self
            .device
            .create_query_set(&crate::resource::QuerySetDescriptor {
                label: None,
                ty: wgt::QueryType::Timestamp,
                count: 1,
            });
        let view = texture.create_view(&Default::default());
        let encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_buffer_to_buffer(source, 0, buffer.clone(), 0, Some(4));
        let mut pass = encoder.begin_render_pass(command::ResolvedRenderPassDescriptor {
            label: None,
            color_attachments: Cow::Owned(vec![Some(command::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                load_op: command::LoadOp::Clear(wgt::Color::BLACK),
                store_op: command::StoreOp::Store,
            })]),
            depth_stencil_attachment: None,
            timestamp_writes: Some(command::PassTimestampWrites {
                query_set: query.clone(),
                beginning_of_pass_write_index: Some(0),
                end_of_pass_write_index: None,
            }),
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.end();
        (encoder.finish(&Default::default()), buffer, texture, query)
    }
}

#[test]
fn shared_receipt_controls_retirement_and_capability_is_captured_once() {
    let fixture = Fixture::new();
    let (commands, buffer, _, _) = fixture.recorded();
    let index = fixture.queue.submit(&[commands])
        .expect("Valid test work must be submitted");
    let events = Arc::new(StdMutex::new(Vec::new()));
    let mapped = events.clone();
    buffer.map_async(
        wgt::MapMode::Read,
        0,
        Some(4),
        Some(Box::new(move |result| {
            assert!(result.is_ok());
            mapped.lock().unwrap().push("map");
        })),
    );
    let done = events.clone();
    fixture
        .queue
        .on_submitted_work_done(Box::new(move || done.lock().unwrap().push("done")));
    fixture.device.poll(wgt::PollType::Poll).unwrap();
    assert!(events.lock().unwrap().is_empty());
    assert!(!fixture.queue.lock_life().queue_empty());
    assert_eq!(fixture.driver.executions_dropped.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .device
            .last_successful_submission_index
            .load(Ordering::SeqCst),
        index
    );
    fixture.driver.ready.store(index, Ordering::SeqCst);
    fixture.device.poll(wgt::PollType::Poll).unwrap();
    assert_eq!(*events.lock().unwrap(), ["map", "done"]);
    assert!(fixture.queue.lock_life().queue_empty());
    assert!(fixture.driver.executions_dropped.load(Ordering::SeqCst) > 0);
    assert_eq!(fixture.driver.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.driver.fatal_captures.load(Ordering::SeqCst), 1);
}

#[test]
fn shared_rejection_does_not_request_context_recovery() {
    let fixture = Fixture::new();
    let (commands, _, _, _) = fixture.recorded();
    fixture.driver.mode.store(REJECT, Ordering::SeqCst);
    fixture.queue.submit(&[commands]);
    fixture.driver.join_probes();
    assert!(!fixture.device.is_valid());
    assert!(fixture.queue.lock_life().queue_empty());
    assert_eq!(
        fixture
            .device
            .last_successful_submission_index
            .load(Ordering::SeqCst),
        0
    );
    assert!(fixture.driver.recoveries.lock().unwrap().is_empty());
    assert!(fixture.driver.unlocked.load(Ordering::SeqCst));
}

#[test]
fn shared_failed_batch_with_confirmed_drain_recovers_without_success() {
    let fixture = Fixture::new();
    let (commands, _, _, _) = fixture.recorded();
    fixture.driver.mode.store(PARTIAL, Ordering::SeqCst);
    fixture.queue.submit(&[commands]);
    fixture.driver.join_probes();
    assert!(!fixture.device.is_valid());
    assert!(fixture.queue.lock_life().queue_empty());
    assert_eq!(
        fixture
            .device
            .last_successful_submission_index
            .load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        *fixture.driver.recoveries.lock().unwrap(),
        [NativeRetirement::Confirmed]
    );
    assert!(fixture.driver.unlocked.load(Ordering::SeqCst));
}

#[test]
fn shared_unconfirmed_batch_keeps_queue_and_destroyed_raw_backing() {
    let fixture = Fixture::new();
    let (commands, buffer, texture, query) = fixture.recorded();
    fixture.driver.mode.store(PARTIAL, Ordering::SeqCst);
    fixture.driver.drain_error.store(true, Ordering::SeqCst);
    assert!(fixture.queue.submit(&[commands]).is_none());
    let index = {
        let life = fixture.queue.lock_life();
        assert_eq!(life.active.len(), 1);
        life.active.last().unwrap().index
    };
    fixture.driver.join_probes();
    assert_eq!(
        unsafe {
            fixture
                .device
                .raw()
                .get_fence_value(fixture.device.fence.as_ref())
        }
        .unwrap(),
        index
    );
    buffer.destroy();
    texture.destroy();
    query.destroy();
    fixture.device.destroy();
    fixture.driver.ready.store(u64::MAX, Ordering::SeqCst);
    fixture.device.poll(wgt::PollType::Poll).unwrap();
    {
        let mut life = fixture.queue.lock_life();
        assert!(life.triage_submissions(u64::MAX).is_empty());
        assert!(!life.queue_empty());
        let active = life.active.last().unwrap();
        assert_eq!(active.index, index);
        assert_eq!(active.completion, Completion::Unconfirmed);
        let resources: Vec<_> = active
            .encoders
            .iter()
            .flat_map(|encoder| encoder.temp_resources.iter())
            .collect();
        assert!(resources
            .iter()
            .any(|resource| matches!(resource, TempResource::DestroyedBuffer(_))));
        assert!(resources
            .iter()
            .any(|resource| matches!(resource, TempResource::DestroyedTexture(_))));
        assert!(resources
            .iter()
            .any(|resource| matches!(resource, TempResource::DestroyedQuerySet(_))));
    }
    assert_eq!(fixture.driver.executions_dropped.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .device
            .last_successful_submission_index
            .load(Ordering::SeqCst),
        0
    );
    assert_eq!(
        *fixture.driver.recoveries.lock().unwrap(),
        [NativeRetirement::Unconfirmed]
    );
    assert!(fixture.driver.unlocked.load(Ordering::SeqCst));
    let weak = Arc::downgrade(&fixture.queue);
    drop(fixture);
    assert!(
        weak.upgrade().is_some(),
        "Fatal quarantine must keep Device.get_queue usable"
    );
}

#[test]
fn shared_poll_failure_quarantines_existing_accepted_owners() {
    let fixture = Fixture::new();
    let (commands, buffer, texture, query) = fixture.recorded();
    let index = fixture.queue.submit(&[commands])
        .expect("Valid test work must be submitted");
    fixture.driver.poll_error.store(true, Ordering::SeqCst);
    fixture.driver.drain_error.store(true, Ordering::SeqCst);
    assert!(fixture.device.poll(wgt::PollType::Poll).is_err());
    fixture.driver.join_probes();
    buffer.destroy();
    texture.destroy();
    query.destroy();
    let life = fixture.queue.lock_life();
    assert!(life.has_unconfirmed());
    assert!(!life.queue_empty());
    assert_eq!(
        fixture
            .device
            .last_successful_submission_index
            .load(Ordering::SeqCst),
        index
    );
    assert_eq!(
        *fixture.driver.recoveries.lock().unwrap(),
        [NativeRetirement::Unconfirmed]
    );
    assert!(fixture.driver.unlocked.load(Ordering::SeqCst));
}

#[test]
fn shared_blocking_poll_shares_one_receipt_timeout_budget() {
    let fixture = Fixture::new();
    fixture.queue.submit(&[]);
    let index = fixture.queue.submit(&[])
        .expect("Valid test progress marker must be submitted");
    fixture.driver.delay_ms.store(40, Ordering::SeqCst);
    let result = fixture.device.poll(wgt::PollType::Wait {
        submission_index: Some(index),
        timeout: Some(Duration::from_millis(60)),
    });
    assert!(matches!(result, Err(crate::device::WaitIdleError::Timeout)));
    let waits = fixture.driver.waits.lock().unwrap().clone();
    assert_eq!(waits.len(), 2);
    assert!(waits[1].unwrap() < waits[0].unwrap());
    fixture.driver.delay_ms.store(0, Ordering::SeqCst);
    fixture
        .device
        .poll(wgt::PollType::Wait {
            submission_index: Some(index),
            timeout: None,
        })
        .unwrap();
    assert!(fixture.queue.lock_life().queue_empty());
}

#[test]
fn shared_device_destroy_waits_for_aggregate_receipt() {
    let fixture = Fixture::new();
    let (commands, _, _, _) = fixture.recorded();
    let index = fixture.queue.submit(&[commands])
        .expect("Valid test work must be submitted");
    let done = fixture.driver.clone();
    fixture
        .queue
        .on_submitted_work_done(Box::new(move || done.events.lock().unwrap().push("done")));
    fixture.device.destroy();
    fixture.device.poll(wgt::PollType::Poll).unwrap();
    assert!(fixture.driver.events.lock().unwrap().is_empty());
    fixture.driver.ready.store(index, Ordering::SeqCst);
    fixture.device.poll(wgt::PollType::Poll).unwrap();
    fixture.driver.join_probes();
    assert_eq!(*fixture.driver.events.lock().unwrap(), ["done", "lost"]);
    assert!(fixture.driver.recoveries.lock().unwrap().is_empty());
    assert!(fixture.driver.unlocked.load(Ordering::SeqCst));
}

#[test]
fn shared_fatal_drop_terminates_the_isolated_process() {
    const CHILD: &str = "WGPU_CORE_SHARED_FATAL_DROP_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let fixture = Fixture::new();
        fixture.queue.submit(&[]);
        fixture.driver.wait_error.store(true, Ordering::SeqCst);
        fixture.driver.drain_error.store(true, Ordering::SeqCst);
        drop(fixture);
        std::process::exit(87);
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "device::life::shared_tests::shared_fatal_drop_terminates_the_isolated_process",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(86),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn shared_wait_for_an_index_gap_still_requires_the_native_fence() {
    let mut life = LifetimeTracker::new();
    life.track_submission(3, Vec::new());
    assert!(life.submission_complete(2));
    let result = crate::device::queue::Queue::shared_poll_status(
        1,
        Some(2),
        life.queue_empty(),
        life.submission_complete(2),
    );
    assert!(matches!(result, Err(crate::device::WaitIdleError::Timeout)));
}

fn canvas_texture(fixture: &Fixture, render: bool) -> Arc<Texture> {
    fixture
        .device
        .create_texture(&crate::resource::TextureDescriptor {
            label: None,
            size: wgt::Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgt::TextureDimension::D2,
            format: wgt::TextureFormat::Rgba8Unorm,
            usage: wgt::TextureUsages::COPY_SRC
                | wgt::TextureUsages::COPY_DST
                | if render {
                    wgt::TextureUsages::RENDER_ATTACHMENT
                } else {
                    wgt::TextureUsages::empty()
                },
            view_formats: Vec::new(),
        })
}

#[test]
fn shared_canvas_no_barrier_still_retains_a_real_finalization_submission() {
    let fixture = Fixture::new();
    let warmup = fixture.queue.submit(&[])
        .expect("Canvas warmup must be submitted");
    fixture.driver.ready.store(warmup, Ordering::SeqCst);
    fixture.device.poll(wgt::PollType::Poll).unwrap();
    assert!(!fixture.queue.pending_writes.lock().is_recording);
    let texture = canvas_texture(&fixture, false);
    texture.initialization_status.write().mips[0].drain(0..1);
    {
        let mut trackers = fixture.device.trackers.lock();
        let transitions: Vec<_> = trackers
            .textures
            .set_single(
                &texture,
                texture.full_range.clone(),
                wgt::TextureUses::RESOURCE,
            )
            .collect();
        assert!(!transitions.is_empty());
    }
    let graphics = fixture
        .device
        .graphics_submission
        .as_ref()
        .unwrap()
        .graphics_queue()
        .unwrap();
    let index = unsafe {
        fixture.queue.finalize_shared_canvas_submission(
            &texture,
            fixture.device.snatchable_lock.read(),
            graphics,
        )
    }
    .unwrap();
    assert!(index > warmup);
    assert!(texture
        .raw(&fixture.device.snatchable_lock.read())
        .is_none());
    assert_eq!(
        fixture
            .queue
            .lock_life()
            .get_texture_latest_submission_index(&texture),
        Some(index)
    );
    assert_eq!(
        fixture.driver.segments.lock().unwrap().last(),
        Some(&(hal::shared::SharedQueueLane::Graphics, true, true))
    );
    fixture.device.poll(wgt::PollType::Poll).unwrap();
    assert!(!fixture.queue.lock_life().queue_empty());
    fixture.driver.ready.store(index, Ordering::SeqCst);
    fixture.device.poll(wgt::PollType::Poll).unwrap();
    assert!(fixture.queue.lock_life().queue_empty());
}

#[test]
fn shared_canvas_local_and_preflight_errors_report_loss_after_guards_drop() {
    for failure in 0..3 {
        let fixture = Fixture::new();
        let texture = canvas_texture(&fixture, true);
        match failure {
            0 => fixture.driver.clear_error.store(true, Ordering::SeqCst),
            1 => fixture.driver.end_error.store(true, Ordering::SeqCst),
            _ => fixture.driver.mode.store(REJECT, Ordering::SeqCst),
        }
        let graphics = fixture
            .device
            .graphics_submission
            .as_ref()
            .unwrap()
            .graphics_queue()
            .unwrap();
        let result = unsafe {
            fixture.queue.finalize_shared_canvas_submission(
                &texture,
                fixture.device.snatchable_lock.read(),
                graphics,
            )
        };
        assert!(result.is_err());
        fixture.driver.join_probes();
        assert!(!fixture.device.is_valid());
        assert!(fixture.driver.unlocked.load(Ordering::SeqCst));
        assert_eq!(
            fixture
                .device
                .last_successful_submission_index
                .load(Ordering::SeqCst),
            0
        );
        assert!(fixture.driver.recoveries.lock().unwrap().is_empty());
        assert!(texture
            .raw(&fixture.device.snatchable_lock.read())
            .is_some());
    }
}

#[derive(Debug)]
struct UnrelatedLease;
impl hal::shared_image::ImageLease for UnrelatedLease {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[test]
fn shared_canvas_without_a_graphics_endpoint_is_rejected_before_mutation() {
    let fixture = Fixture::new();
    let texture = canvas_texture(&fixture, false);
    fixture
        .driver
        .hide_graphics_queue
        .store(true, Ordering::SeqCst);
    assert!(unsafe {
        fixture
            .queue
            .finalize_shared_canvas(&texture, &UnrelatedLease)
    }
    .is_err());
    assert!(fixture.device.is_valid());
    assert_eq!(fixture.driver.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .device
            .last_successful_submission_index
            .load(Ordering::SeqCst),
        0
    );
    assert!(texture
        .raw(&fixture.device.snatchable_lock.read())
        .is_some());
}

#[test]
fn shared_canvas_partial_acceptance_never_expires_or_publishes_the_texture() {
    for unconfirmed in [false, true] {
        let fixture = Fixture::new();
        let texture = canvas_texture(&fixture, true);
        fixture.driver.mode.store(PARTIAL, Ordering::SeqCst);
        fixture
            .driver
            .drain_error
            .store(unconfirmed, Ordering::SeqCst);
        let graphics = fixture
            .device
            .graphics_submission
            .as_ref()
            .unwrap()
            .graphics_queue()
            .unwrap();
        let result = unsafe {
            fixture.queue.finalize_shared_canvas_submission(
                &texture,
                fixture.device.snatchable_lock.read(),
                graphics,
            )
        };
        assert!(result.is_err());
        fixture.driver.join_probes();
        assert!(!fixture.device.is_valid());
        assert!(fixture.driver.unlocked.load(Ordering::SeqCst));
        assert!(texture
            .raw(&fixture.device.snatchable_lock.read())
            .is_some());
        assert_eq!(
            fixture
                .device
                .last_successful_submission_index
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            *fixture.driver.recoveries.lock().unwrap(),
            [if unconfirmed {
                NativeRetirement::Unconfirmed
            } else {
                NativeRetirement::Confirmed
            }]
        );
        if unconfirmed {
            let index = fixture
                .device
                .command_indices
                .read()
                .active_submission_index;
            let mut life = fixture.queue.lock_life();
            assert!(life.triage_submissions(u64::MAX).is_empty());
            assert!(!life.queue_empty());
            assert_eq!(
                life.get_texture_latest_submission_index(&texture),
                Some(index)
            );
            let active = life.active.last().unwrap();
            assert_eq!(active.completion, Completion::Unconfirmed);
            assert!(fixture.device.get_queue().is_some());
            assert_eq!(fixture.driver.executions_dropped.load(Ordering::SeqCst), 0);
        } else {
            assert!(fixture.queue.lock_life().queue_empty());
        }
    }
}

#[test]
fn shared_canvas_wrong_logical_device_is_rejected_before_mutation() {
    let first = Fixture::new();
    let second = Fixture::new();
    let texture = canvas_texture(&first, false);
    assert!(matches!(
        unsafe {
            second
                .queue
                .finalize_shared_canvas(&texture, &UnrelatedLease)
        },
        Err(crate::device::DeviceError::DeviceMismatch(_))
    ));
    assert!(first.device.is_valid() && second.device.is_valid());
    assert_eq!(second.driver.submissions.load(Ordering::SeqCst), 0);
    assert!(texture.raw(&first.device.snatchable_lock.read()).is_some());
}
