/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

renderer_facade!(Renderer, wgpu_hal::api::Vulkan);

pub fn create_vulkan_renderer(
    hal_options: &Options,
    options: WebRenderOptions,
    notifier: Box<dyn RenderNotifier>,
) -> Result<(Renderer, RenderApiSender), String> {
    create_vulkan_renderer_with_compositor(
        hal_options,
        options,
        notifier,
        crate::device::hal::CompositorConfig::Draw,
    )
}

pub fn create_vulkan_renderer_with_compositor(
    hal_options: &Options,
    options: WebRenderOptions,
    notifier: Box<dyn RenderNotifier>,
    compositor: crate::device::hal::CompositorConfig,
) -> Result<(Renderer, RenderApiSender), String> {
    create_renderer::<wgpu_hal::api::Vulkan>(hal_options, options, notifier, compositor, None)
        .map(|(core, sender)| (Renderer { core }, sender))
}

pub fn create_vulkan_renderer_for_window(
    hal_options: &Options,
    options: WebRenderOptions,
    notifier: Box<dyn RenderNotifier>,
    compositor: crate::device::hal::CompositorConfig,
    window: std::rc::Rc<dyn crate::device::hal::SurfaceWindow>,
    size: [u32; 2],
    surface_options: crate::device::hal::SurfaceOptions,
) -> Result<(Renderer, RenderApiSender), String> {
    create_renderer::<wgpu_hal::api::Vulkan>(
        hal_options,
        options,
        notifier,
        compositor,
        Some((window, size, surface_options)),
    )
    .map(|(core, sender)| (Renderer { core }, sender))
}
