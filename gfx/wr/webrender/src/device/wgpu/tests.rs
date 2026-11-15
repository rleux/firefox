/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static VALIDATION_ERRORS: AtomicUsize = AtomicUsize::new(0);
struct Logger;
impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() == log::Level::Error
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            VALIDATION_ERRORS.fetch_add(1, Ordering::Relaxed);
            eprintln!("{}: {}", record.target(), record.args());
        }
    }
    fn flush(&self) {}
}

fn device() -> Device {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        log::set_logger(&Logger).unwrap();
        log::set_max_level(log::LevelFilter::Error);
    });
    let device = Device::new(&Options {
        validation: true,
        ..Default::default()
    })
    .unwrap();
    eprintln!("Core Vulkan adapter: {:?}", device.info());
    assert_eq!(device.info().backend, wgt::Backend::Vulkan);
    assert!(device.adapter.features().contains(device.features()));
    assert_eq!(
        device.features() & wgt::Features::MAPPABLE_PRIMARY_BUFFERS,
        vulkan::primary_buffer_features(device.adapter.features(), device.info().device_type)
    );
    device
}

fn buffer(device: &Device, usage: wgt::BufferUsages, size: u64) -> Arc<core::resource::Buffer> {
    device
        .create_buffer(&core::resource::BufferDescriptor {
            label: Some("core resource test".into()),
            size,
            usage,
            mapped_at_creation: false,
        })
        .unwrap()
}

fn texture_desc() -> core::resource::TextureDescriptor<'static> {
    core::resource::TextureDescriptor {
        label: Some("core texture test".into()),
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
            | wgt::TextureUsages::TEXTURE_BINDING,
        view_formats: vec![],
    }
}

fn finish(encoder: &Arc<core::command::CommandEncoder>) -> Arc<core::command::CommandBuffer> {
    encoder.finish(&wgt::CommandBufferDescriptor {
        label: Some("core resource test".into()),
    })
}

fn idle(device: &Device) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !device.poll().unwrap().is_queue_empty() {
        assert!(Instant::now() < deadline, "Core queue did not complete");
        std::thread::yield_now();
    }
}

fn read(device: &Device, buffer: &Arc<core::resource::Buffer>, size: u64) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.map_async(
        wgt::MapMode::Read,
        0,
        Some(size),
        Some(Box::new(move |result| {
            tx.send(result).unwrap();
        })),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        device.poll().unwrap();
        if let Ok(result) = rx.try_recv() {
            result.unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Core buffer mapping did not complete"
        );
        std::thread::yield_now();
    }
    let bytes = buffer
        .get_mapped_range(0, Some(size))
        .unwrap()
        .read_slice()
        .to_vec();
    buffer.unmap();
    bytes
}

#[test]
fn primary_mapping_is_optional_and_limited_to_shared_memory_adapters() {
    let mapping = wgt::Features::MAPPABLE_PRIMARY_BUFFERS;
    let unrelated = wgt::Features::DUAL_SOURCE_BLENDING;
    for kind in [wgt::DeviceType::IntegratedGpu, wgt::DeviceType::Cpu] {
        assert_eq!(
            vulkan::primary_buffer_features(mapping | unrelated, kind),
            mapping
        );
        assert!(vulkan::primary_buffer_features(unrelated, kind).is_empty());
    }
    for kind in [
        wgt::DeviceType::DiscreteGpu,
        wgt::DeviceType::VirtualGpu,
        wgt::DeviceType::Other,
    ] {
        assert!(vulkan::primary_buffer_features(mapping | unrelated, kind).is_empty());
    }
}

#[test]
fn adapter_selection_and_input_validation() {
    let adapters = vec![
        ("software".into(), wgt::DeviceType::Cpu),
        ("integrated".into(), wgt::DeviceType::IntegratedGpu),
        ("discrete B".into(), wgt::DeviceType::DiscreteGpu),
        ("discrete A".into(), wgt::DeviceType::DiscreteGpu),
    ];
    assert_eq!(select_adapter(&adapters, None).unwrap(), 2);
    assert_eq!(select_adapter(&adapters, Some("INTEGRATED")).unwrap(), 1);
    assert_eq!(select_adapter(&adapters, Some("missing")).unwrap(), 2);
    assert!(select_adapter(&[], None).is_err());
    for name in ["", " ", "\t\n"] {
        assert_eq!(
            Device::new(&Options {
                adapter_name: Some(name.into()),
                ..Default::default()
            })
            .err()
            .unwrap(),
            "Adapter name must not be empty"
        );
    }
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_resources_retain_data_after_caller_drops_handles() {
    let device = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    assert!(device.limits().max_texture_dimension_2d >= 2048);
    for value in 1..=8 {
        let source = buffer(
            &device,
            wgt::BufferUsages::COPY_SRC | wgt::BufferUsages::COPY_DST,
            64,
        );
        let target = buffer(
            &device,
            wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
            64,
        );
        device.queue.write_buffer(source.clone(), 0, &[value; 64]);
        let encoder = device.create_command_encoder("retention test").unwrap();
        encoder.copy_buffer_to_buffer(source.clone(), 0, target.clone(), 0, Some(64));
        let weak = Arc::downgrade(&source);
        drop(source);
        assert!(weak.upgrade().is_some());
        let commands = finish(&encoder);
        drop(encoder);
        let index = device.submit(&[commands.clone()]).unwrap();
        assert!(index > 0);
        drop(commands);
        assert_eq!(read(&device, &target, 64), vec![value; 64]);
        drop(target);
        idle(&device);
        assert!(weak.upgrade().is_none());
    }
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_texture_view_and_submission_own_texture() {
    let device = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let texture = device.create_texture(&texture_desc()).unwrap();
    let weak = Arc::downgrade(&texture);
    let view = texture.create_view(&core::resource::TextureViewDescriptor::default());
    device.check().unwrap();
    let target = buffer(
        &device,
        wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
        512,
    );
    let encoder = device
        .create_command_encoder("texture initialization test")
        .unwrap();
    encoder.copy_texture_to_buffer(
        &wgt::TexelCopyTextureInfo {
            texture: texture.clone(),
            mip_level: 0,
            origin: wgt::Origin3d::ZERO,
            aspect: wgt::TextureAspect::All,
        },
        &wgt::TexelCopyBufferInfo {
            buffer: target.clone(),
            layout: wgt::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: Some(2),
            },
        },
        &texture_desc().size,
    );
    drop(texture);
    assert!(weak.upgrade().is_some());
    drop(view);
    let commands = finish(&encoder);
    drop(encoder);
    device.submit(&[commands.clone()]).unwrap();
    drop(commands);
    assert_eq!(read(&device, &target, 512), vec![0; 512]);
    idle(&device);
    assert!(weak.upgrade().is_none());
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_abandoned_commands_do_not_modify_or_retain_resources() {
    let device = device();
    let before = VALIDATION_ERRORS.load(Ordering::Relaxed);
    let source = buffer(
        &device,
        wgt::BufferUsages::COPY_SRC | wgt::BufferUsages::COPY_DST,
        64,
    );
    device.queue.write_buffer(source.clone(), 0, &[19; 64]);
    device.submit(&[]).unwrap();
    idle(&device);
    for finished in [false, true] {
        let encoder = device.create_command_encoder("abandon test").unwrap();
        encoder.clear_buffer(source.clone(), 0, Some(64));
        if finished {
            drop(finish(&encoder));
        }
        drop(encoder);
    }
    let target = buffer(
        &device,
        wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
        64,
    );
    let encoder = device.create_command_encoder("after abandon test").unwrap();
    encoder.copy_buffer_to_buffer(source.clone(), 0, target.clone(), 0, Some(64));
    device.submit(&[finish(&encoder)]).unwrap();
    drop(encoder);
    assert_eq!(read(&device, &target, 64), vec![19; 64]);
    let orphan = buffer(&device, wgt::BufferUsages::COPY_DST, 64);
    let weak = Arc::downgrade(&orphan);
    let encoder = device.create_command_encoder("abandoned resource").unwrap();
    encoder.clear_buffer(orphan.clone(), 0, None);
    drop(orphan);
    assert!(weak.upgrade().is_some());
    drop(encoder);
    idle(&device);
    assert!(weak.upgrade().is_none());
    assert_eq!(VALIDATION_ERRORS.load(Ordering::Relaxed), before);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_validation_is_distinct_from_device_loss() {
    let device = device();
    let error = device
        .create_buffer(&core::resource::BufferDescriptor {
            label: Some("invalid buffer usage".into()),
            size: 16,
            usage: wgt::BufferUsages::empty(),
            mapped_at_creation: false,
        })
        .err()
        .unwrap();
    assert_eq!(error.kind, ErrorType::Validation);
    assert!(device.device.is_valid());
    assert_eq!(device.submit(&[]).unwrap_err(), error);
    assert_eq!(device.poll().unwrap_err(), error);
    let device = self::device();
    let mut desc = texture_desc();
    desc.size.width = 0;
    assert_eq!(
        device.create_texture(&desc).err().unwrap().kind,
        ErrorType::Validation
    );
    assert!(device.device.is_valid());
    let device = self::device();
    let texture = device.create_texture(&texture_desc()).unwrap();
    texture.create_view(&core::resource::TextureViewDescriptor {
        range: wgt::ImageSubresourceRange {
            base_mip_level: 1,
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(device.check().unwrap_err().kind, ErrorType::Validation);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_submit_error_cannot_be_reported_as_success() {
    let device = device();
    let encoder = device
        .create_command_encoder("duplicate submit test")
        .unwrap();
    let commands = finish(&encoder);
    device.submit(&[commands.clone()]).unwrap();
    assert_eq!(
        device.submit(&[commands]).unwrap_err().kind,
        ErrorType::Validation
    );
    assert!(device.device.is_valid());
    assert_eq!(device.failure().unwrap().kind, ErrorType::Validation);
}

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn native_loss_is_observed_before_submission_and_callbacks_still_run() {
    let idle_device = device();
    idle_device.device.destroy();
    assert_eq!(idle_device.failure().unwrap().kind, ErrorType::DeviceLost);
    assert_eq!(idle_device.poll().unwrap_err().kind, ErrorType::DeviceLost);
    assert!(idle_device
        .failure()
        .unwrap()
        .message
        .starts_with("Destroyed:"));
    let device = device();
    let target = buffer(
        &device,
        wgt::BufferUsages::COPY_DST | wgt::BufferUsages::MAP_READ,
        64,
    );
    let encoder = device.create_command_encoder("loss test").unwrap();
    encoder.clear_buffer(target.clone(), 0, None);
    device.submit(&[finish(&encoder)]).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    target.map_async(
        wgt::MapMode::Read,
        0,
        Some(64),
        Some(Box::new(move |result| {
            tx.send(result).unwrap();
        })),
    );
    device.device.destroy();
    assert_eq!(device.failure().unwrap().kind, ErrorType::DeviceLost);
    assert_eq!(
        device
            .create_buffer(&core::resource::BufferDescriptor {
                label: None,
                size: 4,
                usage: wgt::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
            .err()
            .unwrap()
            .kind,
        ErrorType::DeviceLost
    );
    assert_eq!(device.submit(&[]).unwrap_err().kind, ErrorType::DeviceLost);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut mapping_delivered = false;
    loop {
        assert_eq!(device.poll().unwrap_err().kind, ErrorType::DeviceLost);
        mapping_delivered |= rx.try_recv().is_ok();
        if mapping_delivered && device.failure().unwrap().message.starts_with("Destroyed:") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Mapping callback was abandoned after device loss"
        );
        std::thread::yield_now();
    }
    assert_eq!(device.failure().unwrap().kind, ErrorType::DeviceLost);
}
