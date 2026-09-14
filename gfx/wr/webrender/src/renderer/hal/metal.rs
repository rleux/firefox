/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
renderer_facade!(MetalRenderer, wgpu_hal::api::Metal);

pub fn create_metal_renderer(
    hal_options: &Options,
    options: WebRenderOptions,
    notifier: Box<dyn RenderNotifier>,
    compositor: crate::device::hal::CompositorConfig,
) -> Result<(MetalRenderer, RenderApiSender), String> {
    create_renderer::<wgpu_hal::api::Metal>(hal_options, options, notifier, compositor, None)
        .map(|(core, sender)| (MetalRenderer { core }, sender))
}

pub fn create_metal_renderer_for_window(
    hal_options: &Options,
    options: WebRenderOptions,
    notifier: Box<dyn RenderNotifier>,
    compositor: crate::device::hal::CompositorConfig,
    window: std::rc::Rc<dyn crate::device::hal::SurfaceWindow>,
    size: [u32; 2],
    surface_options: crate::device::hal::SurfaceOptions,
) -> Result<(MetalRenderer, RenderApiSender), String> {
    create_renderer::<wgpu_hal::api::Metal>(
        hal_options,
        options,
        notifier,
        compositor,
        Some((window, size, surface_options)),
    )
    .map(|(core, sender)| (MetalRenderer { core }, sender))
}

pub fn create_metal_renderer_for_layer(
    hal_options: &Options,
    options: WebRenderOptions,
    notifier: Box<dyn RenderNotifier>,
    compositor: crate::device::hal::CompositorConfig,
    layer: objc2::rc::Retained<objc2_quartz_core::CAMetalLayer>,
    size: [u32; 2],
    surface_options: crate::device::hal::SurfaceOptions,
) -> Result<(MetalRenderer, RenderApiSender), String> {
    create_renderer_with_factory::<wgpu_hal::api::Metal>(
        hal_options,
        options,
        notifier,
        compositor,
        || {
            let (device, setup) =
                crate::device::hal::metal::create_device_for_layer(hal_options, layer)?;
            Ok((device, Some((setup, size, surface_options))))
        },
    )
    .map(|(core, sender)| (MetalRenderer { core }, sender))
}
