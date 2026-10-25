use super::*;
use core::sync::atomic::{AtomicBool, Ordering};
use hal::Instance as _;

fn noop_instance() -> hal::noop::Context {
    unsafe {
        hal::noop::Context::init(&hal::InstanceDescriptor {
            name: "core instance import tests",
            flags: InstanceFlags::empty(),
            memory_budget_thresholds: Default::default(),
            backend_options: wgt::BackendOptions {
                noop: wgt::NoopBackendOptions::enabled(),
                ..Default::default()
            },
            telemetry: None,
            display: None,
        })
    }
    .unwrap()
}

fn import(flags: InstanceFlags) -> Arc<Instance> {
    unsafe {
        Instance::from_hal_instance_dynamic(
            "independent core view".into(),
            Backend::Noop,
            Box::new(noop_instance()),
            flags,
            None,
        )
    }
}

fn adapter(instance: &Arc<Instance>) -> Arc<Adapter> {
    instance
        .enumerate_adapters(Backends::NOOP, false)
        .pop()
        .unwrap()
}

#[test]
fn dynamic_import_preserves_validation_and_adapter_consumption_policy() {
    let flags = InstanceFlags::STRICT_WEBGPU_COMPLIANCE
        | InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION
        | InstanceFlags::DISCARD_HAL_LABELS;
    let strict = import(flags);
    let unrestricted = import(InstanceFlags::empty());
    assert_eq!(strict.flags, flags);
    assert_eq!(unrestricted.flags, InstanceFlags::empty());
    assert_eq!(strict.requested_backends, Backends::NOOP);
    assert_eq!(strict.supported_backends, Backends::NOOP);

    let first_adapter = adapter(&strict);
    let first = first_adapter.request_device(&Default::default()).unwrap();
    assert!(matches!(
        first_adapter.request_device(&Default::default()),
        Err(RequestDeviceError::AdapterConsumed)
    ));
    let fresh_adapter = adapter(&strict);
    assert!(!Arc::ptr_eq(&first_adapter, &fresh_adapter));
    let second = fresh_adapter.request_device(&Default::default()).unwrap();

    let permissive_adapter = adapter(&unrestricted);
    let third = permissive_adapter
        .request_device(&Default::default())
        .unwrap();
    let fourth = permissive_adapter
        .request_device(&Default::default())
        .unwrap();
    assert_eq!(
        strict
            .devices
            .0
            .lock()
            .iter()
            .filter_map(|d| d.upgrade())
            .count(),
        2
    );
    assert_eq!(
        unrestricted
            .devices
            .0
            .lock()
            .iter()
            .filter_map(|d| d.upgrade())
            .count(),
        2
    );
    drop((first, second, third, fourth));
}

#[test]
fn dynamic_import_keeps_device_polling_private() {
    let first = import(InstanceFlags::empty());
    let second = import(InstanceFlags::empty());
    let (device, queue) = adapter(&first).request_device(&Default::default()).unwrap();
    let buffer = device.create_buffer(&crate::resource::BufferDescriptor {
        label: None,
        size: 4,
        usage: wgt::BufferUsages::MAP_READ | wgt::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let called = Arc::new(AtomicBool::new(false));
    let result = called.clone();
    buffer.map_async(
        wgt::MapMode::Read,
        0,
        Some(4),
        Some(Box::new(move |status| {
            assert!(status.is_ok(), "{status:?}");
            result.store(true, Ordering::SeqCst);
        })),
    );
    assert!(!called.load(Ordering::SeqCst));
    second.poll_all_devices(false).unwrap();
    assert!(!called.load(Ordering::SeqCst));
    first.poll_all_devices(false).unwrap();
    assert!(called.load(Ordering::SeqCst));
    drop(queue);
}

#[test]
fn generic_instance_import_retains_default_policy() {
    let instance =
        Instance::from_hal_instance::<hal::api::Noop>("legacy core view".into(), noop_instance());
    assert_eq!(instance.flags, InstanceFlags::default());
    assert_eq!(instance.requested_backends, Backends::NOOP);
    assert_eq!(instance.supported_backends, Backends::NOOP);
}
