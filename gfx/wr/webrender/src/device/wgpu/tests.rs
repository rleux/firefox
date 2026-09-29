/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use wgpu_hal::{CommandEncoder as _, Device as _, Queue as _};

#[derive(Debug)]
pub(super) enum Command {
    BeginPass(hal::AttachmentOps, hal::AttachmentOps),
    EndPass,
    SubmissionIdleWait,
    TextureBarrier(wgt::TextureUses, wgt::TextureUses),
    BindGroup,
}

#[test]
fn adapter_preference_preserves_enumeration_order() {
    let adapters = [
        ("software", wgt::DeviceType::Cpu),
        ("integrated", wgt::DeviceType::IntegratedGpu),
        ("discrete B", wgt::DeviceType::DiscreteGpu),
        ("discrete A", wgt::DeviceType::DiscreteGpu),
        ("discrete A", wgt::DeviceType::DiscreteGpu),
    ];
    assert_eq!(select_adapter(adapters.iter().copied(), None).unwrap(), 2);
    assert_eq!(select_adapter(adapters[..2].iter().copied(), None).unwrap(), 1);
    assert_eq!(select_adapter(adapters[..1].iter().copied(), None).unwrap(), 0);
    assert!(select_adapter(std::iter::empty(), None).is_err());
}

#[test]
fn adapter_filter_returns_first_match() {
    let adapters = [
        ("software GPU", wgt::DeviceType::Cpu),
        ("integrated GPU", wgt::DeviceType::IntegratedGpu),
        ("discrete GPU", wgt::DeviceType::DiscreteGpu),
        ("Écran GPU", wgt::DeviceType::VirtualGpu),
    ];
    let requested = "INTEGRATED".to_lowercase();
    assert_eq!(select_adapter(adapters.iter().copied(), Some(&requested)).unwrap(), 1);
    assert_eq!(select_adapter(adapters.iter().copied(), Some("gpu")).unwrap(), 0);
    assert_eq!(select_adapter(adapters.iter().copied(), Some("écran")).unwrap(), 3);
    assert_eq!(select_adapter(adapters.iter().copied(), Some("cran")).unwrap(), 3);
    for name in ["missing", "software gpu too long"] {
        assert_eq!(select_adapter(adapters.iter().copied(), Some(name)).unwrap(), 2);
    }
    let first = std::iter::once(adapters[0]).chain(std::iter::from_fn(|| {
        panic!("Selection must stop after the first name match")
    }));
    assert_eq!(select_adapter(first, Some("gpu")).unwrap(), 0);
}

#[test]
fn adapter_name_validation_precedes_device_initialization() {
    for name in ["", " ", "\t\n"] {
        let result = Device::new(&Options {
            adapter_name: Some(name.into()),
            ..Default::default()
        });
        assert_eq!(result.err().unwrap(), "Adapter name must not be empty");
    }
}

pub(super) static ERRORS: AtomicUsize = AtomicUsize::new(0);
struct TestLogger;

impl log::Log for TestLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() == log::Level::Error
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            ERRORS.fetch_add(1, Ordering::Relaxed);
            eprintln!("{}: {}", record.target(), record.args());
        }
    }

    fn flush(&self) {}
}

pub(super) fn validation_logging() {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        log::set_logger(&TestLogger).unwrap();
        log::set_max_level(log::LevelFilter::Error);
    });
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn open_device_and_submit_to_queue() {
    validation_logging();
    let device = Device::new(&Options {
        validation: true,
        ..Options::default()
    })
    .unwrap();
    eprintln!("Vulkan adapter: {:?}", device.info());
    assert_eq!(device.info().backend, wgt::Backend::Vulkan);
    assert!(device.capabilities().limits.max_texture_dimension_2d >= 2048);
    unsafe {
        let raw = device.open.device.as_ref();
        let fence = raw.create_fence().unwrap();
        device.open.queue.submit(&[], &[], (fence.as_ref(), 1)).unwrap();
        let completed = raw
            .wait(fence.as_ref(), 1, Some(std::time::Duration::from_secs(10)))
            .unwrap();
        if !completed {
            device.queue().wait_for_idle().unwrap();
        }
        raw.destroy_fence(fence);
        assert!(completed);
    }
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
fn buffer_allocation_bounds() {
    use super::resources::allocation_size;
    assert_eq!(allocation_size(0, 256).unwrap(), 4);
    assert_eq!(allocation_size(5, 256).unwrap(), 8);
    assert_eq!(allocation_size(256, 256).unwrap(), 256);
    assert!(allocation_size(257, 256).is_err());
    assert!(allocation_size(0, 3).is_err());
    assert!(allocation_size(usize::MAX, u64::MAX).is_err());
    assert!(allocation_size(isize::MAX as usize, u64::MAX).is_err());
}

pub(super) fn record_upload(
    device: &Rc<Device>,
    source: &Rc<Buffer>,
) -> (Submission, Rc<super::resources::Owned<dyn hal::DynBuffer>>) {
    unsafe {
        let raw = device.open.device.as_ref();
        let (target, _) = raw
            .create_buffer(&hal::BufferDescriptor {
                label: Some("WR upload test readback"),
                size: source.size(),
                usage: wgt::BufferUses::COPY_DST | wgt::BufferUses::MAP_READ,
                memory_flags: hal::MemoryFlags::PREFER_COHERENT,
            })
            .unwrap();
        let target = Rc::new(super::resources::Owned::new(
            device,
            target,
            <dyn hal::DynDevice>::destroy_buffer,
        ));
        let mut submission = Submission::new(device).unwrap();
        let mut commands = submission.recording().unwrap();
        source
            .transition(&mut commands, wgt::BufferUses::COPY_SRC)
            .unwrap();
        let encoder = commands.encoder();
        encoder.copy_buffer_to_buffer(
            &*source.raw,
            &**target,
            &[hal::BufferCopy {
                src_offset: 0,
                dst_offset: 0,
                size: std::num::NonZeroU64::new(source.size()).unwrap(),
            }],
        );
        encoder.transition_buffers(&[hal::BufferBarrier {
            buffer: &**target,
            usage: hal::StateTransition {
                from: wgt::BufferUses::COPY_DST,
                to: wgt::BufferUses::MAP_READ,
            },
        }]);
        source
            .transition(&mut commands, wgt::BufferUses::MAP_WRITE)
            .unwrap();
        commands.keep(&target);
        { drop(commands); (submission, target) }
    }
}

pub(super) fn map_upload(device: &Device, target: &dyn hal::DynBuffer, size: u64) -> Vec<u8> {
    unsafe {
        let raw = device.open.device.as_ref();
        let mapping = raw.map_buffer(target, 0..size).unwrap();
        if !mapping.is_coherent {
            raw.invalidate_mapped_ranges(target, &[0..size]);
        }
        let result = std::slice::from_raw_parts(mapping.ptr.as_ptr(), size as usize).to_vec();
        raw.unmap_buffer(target);
        result
    }
}

fn read_upload(device: &Rc<Device>, source: &Rc<Buffer>) -> Vec<u8> {
    let (mut submission, target) = record_upload(device, source);
    submission.submit().unwrap();
    assert!(submission
        .wait(Some(std::time::Duration::from_secs(10)))
        .unwrap());
    map_upload(device, &**target, source.size())
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn upload_buffers_preserve_data_and_device() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    let failed = Buffer::new_with(&device, 17, wgt::BufferUses::COPY_SRC, |bytes| {
        bytes.fill(42);
        Err("Upload callback failed".into())
    });
    assert!(failed.is_err());
    assert_eq!(Rc::strong_count(&device), 1);

    for length in [0, 3, 129] {
        let bytes: Vec<_> = (0..length).map(|index| (index * 17) as u8).collect();
        let mut buffer = Buffer::new(
            &device,
            &bytes,
            wgt::BufferUses::COPY_SRC | wgt::BufferUses::VERTEX,
        )
        .unwrap();
        let copied = read_upload(&device, &buffer);
        assert_eq!(&copied[..length], &bytes);
        assert!(copied[length..].iter().all(|&byte| byte == 0));
        assert_eq!(buffer.binding_size(), (length as u64).max(4));
        assert!(buffer.vertex_binding(0, buffer.binding_size()).is_ok());
        for (offset, size) in [
            (1, 1),
            (0, 0),
            (0, buffer.binding_size() + 1),
            (u64::MAX - 3, 8),
        ] {
            assert!(buffer.vertex_binding(offset, size).is_err());
        }
        let capacity = buffer.size() as usize;
        assert!(Rc::get_mut(&mut buffer)
            .unwrap()
            .write_with(capacity + 1, |_| {
                panic!("Oversized write reached the callback");
            })
            .is_err());
        Rc::get_mut(&mut buffer)
            .unwrap()
            .write_with(3, |bytes| {
                bytes.copy_from_slice(&[7, 8, 9]);
                Ok(())
            })
            .unwrap();
        assert_eq!(&read_upload(&device, &buffer)[..3], &[7, 8, 9]);
    }
    let buffer = Buffer::new(&device, &[1, 2, 3, 4], wgt::BufferUses::COPY_SRC).unwrap();
    assert!(buffer.vertex_binding(0, 4).is_err());
    let weak_device = Rc::downgrade(&device);
    drop(device);
    assert!(weak_device.upgrade().is_some());
    drop(buffer);
    assert!(weak_device.upgrade().is_none());
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn submission_completion_releases_buffers() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    for poll in [false, true] {
        let bytes: Vec<_> = (0..129).map(|index| (index * 13) as u8).collect();
        let source = Buffer::new(&device, &bytes, wgt::BufferUses::COPY_SRC).unwrap();
        let size = source.size();
        let weak_source = Rc::downgrade(&source);
        let (mut submission, target) = record_upload(&device, &source);
        drop(source);
        assert!(weak_source.upgrade().is_some());
        assert!(submission.poll().is_err());
        assert!(submission.wait(None).is_err());
        submission.submit().unwrap();
        assert!(submission.submit().is_err());
        assert!(submission.recording().is_err());
        assert!(weak_source.upgrade().is_some());
        if poll {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !submission.poll().unwrap() {
                assert!(weak_source.upgrade().is_some());
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
        } else if !submission.wait(Some(std::time::Duration::ZERO)).unwrap() {
            assert!(weak_source.upgrade().is_some());
            assert!(submission
                .wait(Some(std::time::Duration::from_secs(10)))
                .unwrap());
        }
        assert!(weak_source.upgrade().is_none());
        assert!(submission.poll().unwrap());
        assert!(submission.wait(None).unwrap());
        let copied = map_upload(&device, &**target, size);
        assert_eq!(&copied[..bytes.len()], &bytes);
        assert!(copied[bytes.len()..].iter().all(|&byte| byte == 0));
    }
    assert_eq!(Rc::strong_count(&device), 1);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn submission_drop_and_abandon_release_buffers() {
    validation_logging();
    let device = Rc::new(
        Device::new(&Options {
            validation: true,
            ..Options::default()
        })
        .unwrap(),
    );
    eprintln!("Vulkan adapter: {:?}", device.info());
    for submit in [false, true] {
        let bytes = [37; 64];
        let source = Buffer::new(&device, &bytes, wgt::BufferUses::COPY_SRC).unwrap();
        let weak_source = Rc::downgrade(&source);
        let (mut submission, target) = record_upload(&device, &source);
        drop(source);
        assert!(weak_source.upgrade().is_some());
        if submit {
            submission.submit().unwrap();
        }
        drop(submission);
        assert!(weak_source.upgrade().is_none());
        if submit {
            assert_eq!(map_upload(&device, &**target, bytes.len() as u64), bytes);
        }
    }
    assert_eq!(Rc::strong_count(&device), 1);
    drop(device);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[path = "texture_tests.rs"]
mod texture;

#[path = "state_tests.rs"]
mod state;

#[path = "buffer_pool_tests.rs"]
mod buffer_pool;
