use super::*;
use alloc::vec;
#[cfg(feature = "noop")]
use core::cell::Cell;
use core::cell::RefCell;

#[derive(Default)]
struct FaultState {
    active: bool,
    clear_calls: usize,
    fail_at: usize,
    events: Vec<&'static str>,
}
std::thread_local! {
    static FAULT: RefCell<FaultState> = RefCell::new(FaultState::default());
}

struct Fault;
impl Fault {
    fn at(clear_call: usize) -> Self {
        FAULT.with(|fault| {
            *fault.borrow_mut() = FaultState {
                active: true,
                fail_at: clear_call,
                ..Default::default()
            }
        });
        Self
    }
}
impl Drop for Fault {
    fn drop(&mut self) {
        FAULT.with(|fault| *fault.borrow_mut() = FaultState::default());
    }
}

pub(super) fn record(event: &'static str) {
    FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if fault.active {
            fault.events.push(event);
        }
    });
}

pub(super) fn create_clear_view(
    device: &dyn hal::DynDevice,
    texture: &dyn hal::DynTexture,
    descriptor: &hal::TextureViewDescriptor<'_>,
) -> Result<Box<dyn hal::DynTextureView>, hal::DeviceError> {
    let fail = FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if !fault.active {
            return false;
        }
        fault.clear_calls += 1;
        fault.clear_calls == fault.fail_at
    });
    if fail {
        Err(hal::DeviceError::OutOfMemory)
    } else {
        unsafe { device.create_texture_view(texture, descriptor) }
    }
}

#[cfg(feature = "noop")]
struct Metadata;
#[cfg(feature = "noop")]
impl Drop for Metadata {
    fn drop(&mut self) {
        record("allocation");
    }
}

fn descriptor() -> resource::TextureDescriptor<'static> {
    resource::TextureDescriptor {
        label: Some(Cow::Borrowed("shared allocation test")),
        size: wgt::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgt::TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: wgt::TextureUsages::COPY_SRC
            | wgt::TextureUsages::COPY_DST
            | wgt::TextureUsages::RENDER_ATTACHMENT,
        view_formats: vec![TextureFormat::Rgba8UnormSrgb],
    }
}

#[cfg(feature = "noop")]
fn device() -> (Arc<Device>, Arc<crate::device::queue::Queue>) {
    let instance = crate::instance::Instance::new(
        "shared allocation tests",
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
    let mut descriptor = crate::device::DeviceDescriptor::default();
    adapter.validate_device_descriptor(&mut descriptor).unwrap();
    let open = unsafe {
        adapter.as_hal_dynamic().open(
            descriptor.required_features,
            &descriptor.required_limits,
            &descriptor.memory_hints,
        )
    }
    .unwrap();
    unsafe { adapter.create_device_and_queue_from_hal_with_graphics(open, &descriptor, None) }
        .unwrap()
}

#[test]
#[cfg(feature = "noop")]
fn texture_validation_precedes_custom_allocation() {
    let (device, _queue) = device();
    let calls = Cell::new(0);
    for field in 0..4 {
        let mut desc = descriptor();
        match field {
            0 => desc.size.width = 0,
            1 => desc.mip_level_count = 0,
            2 => desc.usage = wgt::TextureUsages::empty(),
            _ => desc.view_formats.push(TextureFormat::Bgra8UnormSrgb),
        }
        let result = device.create_texture_inner::<()>(&desc, |_| {
            calls.set(calls.get() + 1);
            unreachable!("Invalid descriptors must not reach allocation")
        });
        assert!(result.is_err());
    }
    assert_eq!(calls.get(), 0);
}

#[test]
#[cfg(feature = "noop")]
fn custom_allocation_preserves_clear_modes_and_lazy_initialization() {
    let (device, _queue) = device();
    for (format, view_format) in [
        (TextureFormat::Rgba8Unorm, TextureFormat::Rgba8UnormSrgb),
        (TextureFormat::Bgra8Unorm, TextureFormat::Bgra8UnormSrgb),
    ] {
        for render in [true, false] {
            let mut desc = descriptor();
            desc.format = format;
            desc.view_formats = vec![view_format];
            if !render {
                desc.usage = wgt::TextureUsages::COPY_SRC | wgt::TextureUsages::COPY_DST;
            }
            let original = device.create_texture(&desc);
            let (custom, metadata) = device
                .create_texture_inner(&desc, |hal_desc| {
                    assert_eq!(hal_desc.size, desc.size);
                    assert_eq!(hal_desc.format, desc.format);
                    assert_eq!(hal_desc.view_formats, desc.view_formats);
                    assert!(!hal_desc.usage.contains(wgt::TextureUses::RESOURCE));
                    unsafe { device.raw().create_texture(hal_desc) }
                        .map(|raw| (raw, 17))
                        .map_err(|error| device.handle_hal_error_with_nonfatal_oom(error).into())
                })
                .unwrap();
            assert_eq!(metadata, 17);
            for texture in [&original, &custom] {
                let clear = texture.clear_mode.read();
                if render {
                    assert!(
                        matches!(&*clear, resource::TextureClearMode::RenderPass { clear_views, .. } if clear_views.len() == 1)
                    );
                } else {
                    assert!(matches!(&*clear, resource::TextureClearMode::BufferCopy));
                }
                drop(clear);
                assert_eq!(
                    texture.initialization_status.read().mips[0].check(0..1),
                    Some(0..1)
                );
                assert_eq!(texture.descriptor().usage, desc.usage);
                assert_eq!(texture.descriptor().view_formats, desc.view_formats);
                let mut tracker = device.trackers.lock();
                let mut pending = tracker.textures.set_single(
                    texture,
                    texture.full_range.clone(),
                    wgt::TextureUses::COPY_SRC,
                );
                let transition = pending
                    .next()
                    .expect("Initial tracker state must require a transition");
                assert_eq!(transition.usage.from, wgt::TextureUses::UNINITIALIZED);
                drop(pending);
            }
        }
    }
}

#[test]
#[cfg(feature = "noop")]
fn partial_clear_failure_destroys_views_before_texture_and_metadata() {
    let (device, _queue) = device();
    let mut desc = descriptor();
    desc.mip_level_count = 2;
    let _fault = Fault::at(2);
    let result = device.create_texture_inner(&desc, |hal_desc| {
        unsafe { device.raw().create_texture(hal_desc) }
            .map(|raw| (raw, Metadata))
            .map_err(|error| device.handle_hal_error_with_nonfatal_oom(error).into())
    });
    assert!(matches!(
        result,
        Err(resource::CreateTextureError::Device(
            DeviceError::OutOfMemory
        ))
    ));
    FAULT.with(|fault| {
        let fault = fault.borrow();
        assert_eq!(fault.clear_calls, 2);
        assert_eq!(fault.events, ["view", "texture", "allocation"]);
    });
}

#[test]
#[cfg(feature = "noop")]
fn trusted_canvas_validation_preserves_error_scopes_and_returns_no_lease() {
    let (device, _queue) = device();
    let graphics = graphics(&device);
    let mut texture_desc = descriptor();
    texture_desc
        .view_formats
        .push(TextureFormat::Bgra8UnormSrgb);
    device.push_error_scope(wgt::error::ErrorFilter::Validation);
    let (texture, lease) =
        unsafe { device.create_texture_with_shared_image(&texture_desc, graphics) };
    assert!(lease.is_none());
    assert!(matches!(&texture.state, ResourceState::Invalid));
    assert!(device.pop_error_scope().unwrap().is_some());
    device.check_is_valid().unwrap();
}

#[test]
#[cfg(vulkan)]
#[ignore = "Requires Vulkan validation and wgpu-types/counters"]
fn partial_clear_failure_releases_native_alias_and_physical_lease() {
    let counter = wgt::InternalCounter::new();
    counter.add(1);
    assert_eq!(
        counter.read(),
        1,
        "Native cleanup test requires wgpu-types/counters"
    );
    let instance = crate::instance::Instance::new(
        "native shared allocation cleanup",
        wgt::InstanceDescriptor {
            backends: wgt::Backends::VULKAN,
            flags: wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION,
            backend_options: Default::default(),
            memory_budget_thresholds: Default::default(),
            display: None,
        },
        None,
    );
    let adapter = instance
        .enumerate_adapters(wgt::Backends::VULKAN, false)
        .into_iter()
        .find(|adapter| adapter.get_info().device_type != wgt::DeviceType::Cpu)
        .unwrap();
    let mut device_desc = crate::device::DeviceDescriptor::default();
    adapter
        .validate_device_descriptor(&mut device_desc)
        .unwrap();
    let pair = unsafe {
        hal::shared::open_shared_device(
            adapter.as_hal_dynamic(),
            device_desc.required_features,
            &device_desc.required_limits,
            &device_desc.memory_hints,
        )
    }
    .unwrap();
    let graphics = Arc::new(pair.graphics);
    assert!(
        hal::renderer::graphics_capabilities(
            instance.raw(wgt::Backend::Vulkan).unwrap(),
            adapter.as_hal_dynamic(),
            graphics.device.as_ref()
        )
        .unwrap()
        .validation_enabled
    );
    let (device, queue) = unsafe {
        adapter.create_device_and_queue_from_hal_with_graphics(pair.compute, &device_desc, None)
    }
    .unwrap();
    let lease_weak = RefCell::new(None);
    let mut desc = descriptor();
    desc.mip_level_count = 2;
    let _fault = Fault::at(2);
    let result = device.create_texture_inner(&desc, |hal_desc| {
        let lease = unsafe { hal::shared_image::allocate(graphics.clone(), hal_desc) }.unwrap();
        *lease_weak.borrow_mut() = Some(Arc::downgrade(&lease));
        let counters = graphics.device.get_internal_counters();
        assert_eq!(counters.textures.read(), 1);
        assert!(counters.texture_memory.read() > 0);
        let raw = unsafe { hal::shared_image::create_alias(lease.clone(), device.raw(), hal_desc) }
            .unwrap();
        Ok((raw, lease))
    });
    assert!(matches!(
        result,
        Err(resource::CreateTextureError::Device(
            DeviceError::OutOfMemory
        ))
    ));
    assert!(lease_weak.borrow().as_ref().unwrap().upgrade().is_none());
    let physical = graphics.device.get_internal_counters();
    let logical = device.get_hal_counters();
    assert_eq!(physical.textures.read(), 0);
    assert_eq!(physical.texture_memory.read(), 0);
    assert_eq!(logical.textures.read(), 0);
    assert_eq!(logical.texture_views.read(), 0);
    pair.context.drain().unwrap();
    drop((queue, device, graphics, pair.context));
}

#[cfg(feature = "noop")]
fn graphics(device: &Device) -> Arc<hal::DynOpenDevice> {
    Arc::new(
        unsafe {
            device.adapter.as_hal_dynamic().open(
                *device.features(),
                device.limits(),
                &wgt::MemoryHints::default(),
            )
        }
        .unwrap(),
    )
}

#[test]
#[cfg(feature = "noop")]
fn trusted_canvas_admission_rejects_other_valid_textures_before_allocation() {
    for field in 0..6 {
        let (device, _queue) = device();
        let graphics = graphics(&device);
        let mut desc = descriptor();
        match field {
            0 => {
                desc.format = TextureFormat::Rgba16Float;
                desc.view_formats.clear();
            }
            1 => {
                desc.dimension = wgt::TextureDimension::D3;
                desc.usage = wgt::TextureUsages::COPY_SRC | wgt::TextureUsages::COPY_DST;
            }
            2 => desc.size.depth_or_array_layers = 2,
            3 => desc.mip_level_count = 2,
            4 => {
                desc.sample_count = 4;
                desc.usage = wgt::TextureUsages::RENDER_ATTACHMENT;
            }
            _ => {
                desc.usage = wgt::TextureUsages::RENDER_ATTACHMENT
                    | wgt::TextureUsages::TRANSIENT_ATTACHMENT;
                desc.view_formats.clear();
            }
        }
        device.validate_texture_descriptor(&desc).unwrap();
        let _fault = Fault::at(usize::MAX);
        let (texture, lease) = unsafe { device.create_texture_with_shared_image(&desc, graphics) };
        assert!(lease.is_none());
        assert!(matches!(&texture.state, ResourceState::Invalid));
        FAULT.with(|fault| {
            assert!(
                fault.borrow().events.is_empty(),
                "Admission field {field} reached allocation"
            )
        });
    }
}
