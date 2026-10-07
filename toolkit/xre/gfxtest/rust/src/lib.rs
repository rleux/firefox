/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use raw_window_handle::{RawDisplayHandle, RawWindowHandle, XlibDisplayHandle, XlibWindowHandle};
use std::{
    ffi::{c_char, c_void},
    ptr::{self, NonNull},
};
use wgpu_hal::{self as hal, Adapter as _, Instance as _};
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
    fn for_surface(
        adapter: &hal::ExposedAdapter<hal::api::Vulkan>,
        surface: hal::SurfaceCapabilities,
    ) -> Self {
        Self {
            device_type: adapter.info.device_type,
            max_texture_dimension_2d: adapter.capabilities.limits.max_texture_dimension_2d,
            swapchain: adapter
                .adapter
                .physical_device_capabilities()
                .supports_extension(c"VK_KHR_swapchain"),
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

    fn failure(&self) -> Option<&'static str> {
        if self.device_type == wgt::DeviceType::Cpu {
            return Some("Vulkan WebRender requires a hardware adapter");
        }
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
        if !self.surface.usage.contains(wgt::TextureUses::COLOR_TARGET)
            || !self
                .surface
                .composite_alpha_modes
                .contains(&wgt::CompositeAlphaMode::Opaque)
        {
            return Some("Vulkan surface lacks direct opaque rendering support");
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
    (device_rank(candidate.device_type), candidate.name.as_str())
        < (device_rank(selected.device_type), selected.name.as_str())
}

pub struct WrStartupProbe {
    selected: Option<hal::ExposedAdapter<hal::api::Vulkan>>,
    instance: hal::vulkan::Instance,
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
            instance,
        })
    }

    unsafe fn surface(
        &self,
        display: *mut c_void,
        window: u64,
        screen: i32,
    ) -> Result<hal::vulkan::Surface, String> {
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
        self.selected = None;
        // shortcut: instance/device version probing is postponed; assume the minimum
        // until a portable HAL query exists. Renderer runtime guards remain enabled.
        let mut best: Option<(
            hal::ExposedAdapter<hal::api::Vulkan>,
            hal::SurfaceCapabilities,
        )> = None;
        for exposed in unsafe { self.instance.enumerate_adapters(Some(&surface)) } {
            let Some(caps) = (unsafe { exposed.adapter.surface_capabilities(&surface) }) else {
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
        match requirements.failure() {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    fn finish(&self) -> Result<(), String> {
        let selected = self.selected.as_ref().ok_or("No selected Vulkan adapter")?;
        let features = selected.features
            & (wgt::Features::DUAL_SOURCE_BLENDING
                | wgt::Features::FLOAT32_FILTERABLE
                | wgt::Features::TEXTURE_FORMAT_16BIT_NORM);
        let hal::OpenDevice { device, queue } = unsafe {
            selected.adapter.open(
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
