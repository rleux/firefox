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

static ERRORS: AtomicUsize = AtomicUsize::new(0);
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

fn validation_logging() {
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

fn read_upload(device: &Device, source: &Buffer) -> Vec<u8> {
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
        let mut encoder = raw
            .create_command_encoder(&hal::CommandEncoderDescriptor {
                label: Some("WR upload test"),
                queue: device.queue(),
            })
            .unwrap();
        encoder.begin_encoding(None).unwrap();
        encoder.transition_buffers(&[hal::BufferBarrier {
            buffer: &*source.raw,
            usage: hal::StateTransition {
                from: wgt::BufferUses::MAP_WRITE,
                to: wgt::BufferUses::COPY_SRC,
            },
        }]);
        encoder.copy_buffer_to_buffer(
            &*source.raw,
            &*target,
            &[hal::BufferCopy {
                src_offset: 0,
                dst_offset: 0,
                size: std::num::NonZeroU64::new(source.size()).unwrap(),
            }],
        );
        encoder.transition_buffers(&[
            hal::BufferBarrier {
                buffer: &*target,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::COPY_DST,
                    to: wgt::BufferUses::MAP_READ,
                },
            },
            hal::BufferBarrier {
                buffer: &*source.raw,
                usage: hal::StateTransition {
                    from: wgt::BufferUses::COPY_SRC,
                    to: wgt::BufferUses::MAP_WRITE,
                },
            },
        ]);
        let commands = encoder.end_encoding().unwrap();
        let fence = raw.create_fence().unwrap();
        device.open.queue
            .submit(&[commands.as_ref()], &[], (fence.as_ref(), 1))
            .unwrap();
        let completed = raw
            .wait(fence.as_ref(), 1, Some(std::time::Duration::from_secs(10)))
            .unwrap();
        if !completed {
            device.queue().wait_for_idle().unwrap();
        }
        let mapping = raw.map_buffer(&*target, 0..source.size()).unwrap();
        if !mapping.is_coherent {
            raw.invalidate_mapped_ranges(&*target, &[0..source.size()]);
        }
        let result =
            std::slice::from_raw_parts(mapping.ptr.as_ptr(), source.size() as usize).to_vec();
        raw.unmap_buffer(&*target);
        encoder.reset_all(vec![commands]);
        drop(encoder);
        raw.destroy_fence(fence);
        raw.destroy_buffer(target);
        assert!(completed);
        result
    }
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
