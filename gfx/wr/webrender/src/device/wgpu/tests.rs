/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use wgpu_hal::{Device as _, Queue as _};

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

#[test]
#[ignore = "Requires Vulkan and the Khronos validation layer"]
fn open_device_and_submit_to_queue() {
    log::set_logger(&TestLogger).unwrap();
    log::set_max_level(log::LevelFilter::Error);
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
