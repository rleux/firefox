/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use wgpu_hal::{Device as _, Queue as _};

#[test]
fn adapter_preference_and_name_tiebreak() {
    let adapters = [
        ("software", wgt::DeviceType::Cpu),
        ("integrated", wgt::DeviceType::IntegratedGpu),
        ("discrete B", wgt::DeviceType::DiscreteGpu),
        ("discrete A", wgt::DeviceType::DiscreteGpu),
    ];
    assert_eq!(select_adapter(&adapters, None).unwrap(), 3);
    assert_eq!(select_adapter(&adapters[..2], None).unwrap(), 1);
    assert_eq!(select_adapter(&adapters[..1], None).unwrap(), 0);
    assert!(select_adapter(&[], None).is_err());
}

#[test]
fn adapter_filter_requires_one_match() {
    let adapters = [
        ("discrete GPU", wgt::DeviceType::DiscreteGpu),
        ("integrated GPU", wgt::DeviceType::IntegratedGpu),
    ];
    assert_eq!(select_adapter(&adapters, Some("INTEGRATED")).unwrap(), 1);
    for name in ["", " ", "missing", "GPU"] {
        assert!(select_adapter(&adapters, Some(name)).is_err(), "{:?}", name);
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
        let raw = device.raw_device();
        let fence = raw.create_fence().unwrap();
        device.queue().submit(&[], &[], (&fence, 1)).unwrap();
        let completed = raw
            .wait(&fence, 1, Some(std::time::Duration::from_secs(10)))
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
