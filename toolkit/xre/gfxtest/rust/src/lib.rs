/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use raw_window_handle::{RawDisplayHandle, RawWindowHandle, XlibDisplayHandle, XlibWindowHandle};
use std::{
    ffi::{c_char, c_void, CStr},
    ptr::{self, NonNull},
};
use wgpu_hal::{self as hal, Instance as _};
use wgpu_types as wgt;

struct Requirements {
    device_type: wgt::DeviceType,
    max_texture_dimension_2d: u32,
    swapchain: bool,
    color: hal::TextureFormatCapabilities,
    depth: hal::TextureFormatCapabilities,
    surface: hal::SurfaceCapabilities,
}

impl Requirements {
    fn for_surface(adapter: &hal::DynExposedAdapter, surface: hal::SurfaceCapabilities) -> Self {
        Self {
            device_type: adapter.info.device_type,
            max_texture_dimension_2d: adapter.capabilities.limits.max_texture_dimension_2d,
            swapchain: adapter
                .adapter
                .as_any()
                .downcast_ref::<hal::vulkan::Adapter>()
                .is_some_and(|adapter| {
                    adapter
                        .physical_device_capabilities()
                        .supports_extension(c"VK_KHR_swapchain")
                }),
            color: unsafe {
                adapter
                    .adapter
                    .texture_format_capabilities(wgt::TextureFormat::Rgba8Unorm)
            },
            depth: unsafe {
                adapter
                    .adapter
                    .texture_format_capabilities(wgt::TextureFormat::Depth32Float)
            },
            surface,
        }
    }

    fn failure(&self, transparent: bool) -> Option<&'static str> {
        if self.device_type == wgt::DeviceType::Cpu {
            return Some("Vulkan WebRender requires a hardware adapter");
        }
        self.surface_failure(transparent)
    }

    fn surface_failure(&self, transparent: bool) -> Option<&'static str> {
        if !self.swapchain {
            return Some("Vulkan adapter lacks VK_KHR_swapchain");
        }
        if !self.color.contains(
            hal::TextureFormatCapabilities::COLOR_ATTACHMENT
                | hal::TextureFormatCapabilities::COPY_SRC,
        ) || !self.depth.contains(
            hal::TextureFormatCapabilities::DEPTH_STENCIL_ATTACHMENT
                | hal::TextureFormatCapabilities::COPY_SRC,
        ) {
            return Some("Vulkan adapter lacks required texture format usages");
        }
        let alpha_supported = if transparent {
            self.surface.composite_alpha_modes.iter().any(|mode| {
                matches!(
                    mode,
                    wgt::CompositeAlphaMode::PreMultiplied | wgt::CompositeAlphaMode::Inherit
                )
            })
        } else {
            self.surface
                .composite_alpha_modes
                .contains(&wgt::CompositeAlphaMode::Opaque)
        };
        if !self.surface.usage.contains(wgt::TextureUses::COLOR_TARGET) || !alpha_supported {
            return Some(if transparent {
                "Vulkan surface lacks direct alpha rendering support"
            } else {
                "Vulkan surface lacks direct opaque rendering support"
            });
        }
        let direct_format = self.surface.formats.iter().any(|entry| {
            matches!(
                entry.format,
                wgt::TextureFormat::Rgba8Unorm | wgt::TextureFormat::Bgra8Unorm
            ) && entry.color_spaces.contains(wgt::SurfaceColorSpaces::SRGB)
        });
        if !direct_format || !self.surface.present_modes.contains(&wgt::PresentMode::Fifo) {
            return Some("Vulkan surface lacks UNORM/sRGB or FIFO presentation");
        }
        if let Some(extent) = self.surface.current_extent {
            if extent.width == 0
                || extent.height == 0
                || extent.width > self.max_texture_dimension_2d
                || extent.height > self.max_texture_dimension_2d
            {
                return Some("Vulkan surface extent is unavailable or exceeds device limits");
            }
        }
        None
    }
}

fn device_rank(kind: wgt::DeviceType) -> u8 {
    match kind {
        wgt::DeviceType::DiscreteGpu => 0,
        wgt::DeviceType::IntegratedGpu => 1,
        wgt::DeviceType::VirtualGpu => 2,
        wgt::DeviceType::Cpu => 3,
        wgt::DeviceType::Other => 4,
    }
}

fn prefer_adapter(candidate: &wgt::AdapterInfo, selected: &wgt::AdapterInfo) -> bool {
    device_rank(candidate.device_type) < device_rank(selected.device_type)
}

fn shared_descriptor(
    hal_labels: bool,
    backends: &str,
) -> Result<hal::InstanceDescriptor<'static>, String> {
    if !backends.is_empty()
        && !wgt::Backends::from_comma_list(backends).contains(wgt::Backends::VULKAN)
    {
        return Err("Unified WebGPU requires the Vulkan backend".into());
    }
    let mut flags = (wgt::InstanceFlags::from_build_config()
        | wgt::InstanceFlags::AUTOMATIC_TIMESTAMP_NORMALIZATION
        | wgt::InstanceFlags::STRICT_WEBGPU_COMPLIANCE)
        .with_env();
    if !hal_labels {
        flags.insert(wgt::InstanceFlags::DISCARD_HAL_LABELS);
    }
    Ok(hal::InstanceDescriptor {
        name: "Unified WebRender/WebGPU",
        flags,
        memory_budget_thresholds: wgt::MemoryBudgetThresholds {
            for_resource_creation: Some(95),
            for_device_loss: Some(99),
        },
        backend_options: Default::default(),
        telemetry: None,
        display: None,
    })
}

pub struct WrStartupProbe {
    selected: Option<hal::DynExposedAdapter>,
    instance: Box<dyn hal::DynInstance>,
    shared: Option<hal::shared::SharedDeviceEndpoints>,
}

impl WrStartupProbe {
    fn new() -> Result<Self, String> {
        let instance = unsafe {
            hal::vulkan::Instance::init(&hal::InstanceDescriptor {
                name: "Vulkan WebRender probe",
                flags: wgt::InstanceFlags::empty(),
                memory_budget_thresholds: Default::default(),
                backend_options: Default::default(),
                telemetry: None,
                display: None,
            })
        }
        .map_err(|error| format!("Cannot initialize Vulkan HAL: {error}"))?;
        Ok(Self {
            selected: None,
            instance: Box::new(instance),
            shared: None,
        })
    }

    fn new_shared(
        hal_labels: bool,
        require_hardware: bool,
        backends: &str,
    ) -> Result<Self, String> {
        let descriptor = shared_descriptor(hal_labels, backends)?;
        let shared =
            unsafe { hal::renderer::bootstrap_shared_device(&descriptor, require_hardware) }?;
        let (_, instance) = shared.context.clone_instance();
        let mut adapters = unsafe { instance.enumerate_adapters(None) };
        if adapters.len() != 1 {
            return Err("Shared owner did not expose its selected adapter".into());
        }
        Ok(Self {
            selected: adapters.pop(),
            instance,
            shared: Some(shared),
        })
    }

    unsafe fn surface(
        &self,
        display: *mut c_void,
        window: u64,
        screen: i32,
    ) -> Result<Box<dyn hal::DynSurface>, String> {
        let display = NonNull::new(display).ok_or("Missing X11 display")?;
        let window = window.try_into().map_err(|_| "Invalid X11 window")?;
        if window == 0 || screen < 0 {
            return Err("Invalid X11 window or screen".into());
        }
        unsafe {
            self.instance.create_surface(
                RawDisplayHandle::Xlib(XlibDisplayHandle::new(Some(display), screen)),
                RawWindowHandle::Xlib(XlibWindowHandle::new(window)),
            )
        }
        .map_err(|error| format!("Cannot create Vulkan X11 surface: {error}"))
    }

    unsafe fn select_xlib(
        &mut self,
        display: *mut c_void,
        window: u64,
        screen: i32,
    ) -> Result<(), String> {
        let surface = unsafe { self.surface(display, window, screen) }?;
        if self.shared.is_some() {
            let selected = self.selected.as_ref().ok_or("No shared adapter")?;
            let caps = unsafe { selected.adapter.surface_capabilities(surface.as_ref()) }
                .ok_or("Shared adapter cannot present to this X11 surface")?;
            return Requirements::for_surface(selected, caps)
                .surface_failure(false)
                .map_or(Ok(()), |error| Err(error.into()));
        }
        self.selected = None;
        // shortcut: instance/device version probing is postponed; assume the minimum
        // until a portable HAL query exists. Renderer runtime guards remain enabled.
        let mut best: Option<(hal::DynExposedAdapter, hal::SurfaceCapabilities)> = None;
        for exposed in unsafe { self.instance.enumerate_adapters(Some(surface.as_ref())) } {
            let Some(caps) = (unsafe { exposed.adapter.surface_capabilities(surface.as_ref()) })
            else {
                continue;
            };
            if best
                .as_ref()
                .is_none_or(|(selected, _)| prefer_adapter(&exposed.info, &selected.info))
            {
                best = Some((exposed, caps));
            }
        }
        let (selected, caps) = best.ok_or("No HAL Vulkan graphics adapter can present to X11")?;
        let requirements = Requirements::for_surface(&selected, caps);
        self.selected = Some(selected);
        match requirements.failure(false) {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    unsafe fn alpha_xlib(&self, display: *mut c_void, window: u64, screen: i32) -> bool {
        let Some(selected) = self.selected.as_ref() else {
            return false;
        };
        let Ok(surface) = (unsafe { self.surface(display, window, screen) }) else {
            return false;
        };
        let Some(caps) = (unsafe { selected.adapter.surface_capabilities(surface.as_ref()) })
        else {
            return false;
        };
        let requirements = Requirements::for_surface(selected, caps);
        if self.shared.is_some() {
            requirements.surface_failure(true).is_none()
        } else {
            requirements.failure(true).is_none()
        }
    }

    fn finish(&self) -> Result<(), String> {
        if self.shared.is_some() {
            return Ok(());
        }
        let selected = self.selected.as_ref().ok_or("No selected Vulkan adapter")?;
        let features = selected.features
            & (wgt::Features::DUAL_SOURCE_BLENDING
                | wgt::Features::FLOAT32_FILTERABLE
                | wgt::Features::TEXTURE_FORMAT_16BIT_NORM);
        let (hal::DynOpenDevice { device, queue }, _) = unsafe {
            hal::renderer::open_device(
                selected.adapter.as_ref(),
                features,
                &selected.capabilities.limits,
                &wgt::MemoryHints::default(),
            )
        }
        .map_err(|error| format!("Cannot create Vulkan renderer device: {error:?}"))?;
        drop(queue);
        drop(device);
        Ok(())
    }
}

#[repr(C)]
pub struct WrStartupProbeReport {
    pub has_device: bool,
    pub device_name: [c_char; 256],
    pub error: [c_char; 256],
}

impl Default for WrStartupProbeReport {
    fn default() -> Self {
        Self {
            has_device: false,
            device_name: [0; 256],
            error: [0; 256],
        }
    }
}

fn copy_bytes(output: &mut [c_char; 256], bytes: &[u8]) {
    output.fill(0);
    for (target, source) in output[..255].iter_mut().zip(bytes) {
        *target = *source as c_char;
    }
}

fn report(
    probe: &WrStartupProbe,
    result: Result<(), String>,
    output: &mut WrStartupProbeReport,
) -> bool {
    *output = WrStartupProbeReport::default();
    if let Some(selected) = probe.selected.as_ref() {
        output.has_device = true;
        copy_bytes(&mut output.device_name, selected.info.name.as_bytes());
    }
    match result {
        Ok(()) => true,
        Err(error) => {
            copy_bytes(&mut output.error, error.as_bytes());
            false
        }
    }
}

#[no_mangle]
pub extern "C" fn wr_startup_probe_new(output: &mut WrStartupProbeReport) -> *mut WrStartupProbe {
    *output = WrStartupProbeReport::default();
    match WrStartupProbe::new() {
        Ok(probe) => Box::into_raw(Box::new(probe)),
        Err(error) => {
            copy_bytes(&mut output.error, error.as_bytes());
            ptr::null_mut()
        }
    }
}

/// # Safety
/// Backends is null or a borrowed NUL-terminated string valid for this call.
#[no_mangle]
pub unsafe extern "C" fn wr_startup_probe_new_shared(
    hal_labels: bool,
    require_hardware: bool,
    backends: *const c_char,
    output: &mut WrStartupProbeReport,
) -> *mut WrStartupProbe {
    *output = WrStartupProbeReport::default();
    let backends = if backends.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(backends) }
            .to_string_lossy()
            .into_owned()
    };
    match WrStartupProbe::new_shared(hal_labels, require_hardware, &backends) {
        Ok(probe) => Box::into_raw(Box::new(probe)),
        Err(error) => {
            copy_bytes(&mut output.error, error.as_bytes());
            ptr::null_mut()
        }
    }
}

/// # Safety
/// Display/window are borrowed live Xlib objects; the probe is exclusively owned.
#[no_mangle]
pub unsafe extern "C" fn wr_startup_probe_select_xlib(
    probe: &mut WrStartupProbe,
    display: *mut c_void,
    window: u64,
    screen: i32,
    output: &mut WrStartupProbeReport,
) -> bool {
    let result = unsafe { probe.select_xlib(display, window, screen) };
    report(probe, result, output)
}

/// # Safety
/// Display/window are borrowed live Xlib objects for this call only.
#[no_mangle]
pub unsafe extern "C" fn wr_startup_probe_alpha_xlib(
    probe: &WrStartupProbe,
    display: *mut c_void,
    window: u64,
    screen: i32,
) -> bool {
    unsafe { probe.alpha_xlib(display, window, screen) }
}

#[no_mangle]
pub extern "C" fn wr_startup_probe_finish(
    probe: &WrStartupProbe,
    output: &mut WrStartupProbeReport,
) -> bool {
    report(probe, probe.finish(), output)
}

/// # Safety
/// The pointer is null or uniquely owned and no call is in progress.
#[no_mangle]
pub unsafe extern "C" fn wr_startup_probe_delete(probe: *mut WrStartupProbe) {
    if !probe.is_null() {
        drop(unsafe { Box::from_raw(probe) });
    }
}

#[cfg(test)]
mod tests;
