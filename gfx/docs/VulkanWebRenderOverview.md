# Vulkan WebRender

WebRender has an opt-in Vulkan backend beneath its shared renderer. Scene building,
batching, render tasks, texture-cache management and composition use the same
renderer as OpenGL. The Vulkan device implementation records native work through
Firefox's pinned `wgpu-hal` dependency.

The startup preference `gfx.webrender.vulkan` defaults to `false`. It requires a
build with Vulkan enabled, hardware WebRender and a supported window system.
Restart Firefox after changing it. `about:support` reports the API and device that
actually started, including software fallback.

## Architecture

| Layer | Responsibility |
| --- | --- |
| Gecko and `RenderCompositorVulkan` | Window ownership, visibility, frame completion and recovery |
| Shared `Renderer` and `Device` | Scene/resource updates, render tasks, draw state and backend selection |
| Vulkan `RenderDevice` | Native resources, pipelines, barriers, submissions and direct presentation |
| `wgpu-hal` | Vulkan device, queue and resource operations |

`Device::new` constructs the backend from `GpuBackendConfig`. WebRender owns its
Vulkan logical device; WebGPU producers have separate devices and use explicit
image-publication and synchronization protocols.

Final composition renders directly into an acquired swapchain image. Picture
caches and other intermediate render targets remain part of ordinary WebRender
rendering. Presentation does not require a full-frame copy from a retained output
texture.

## Scope

The implementation covers ordinary rendering resources, text, images, clips,
gradients, filters, YUV sampling and native-window presentation. CPU images use
the upload path. Linux WebGPU and GL producers can publish compatible RGB DMA-BUF
images for direct sampling. Those paths still have producer-side work and do not
make the whole browser pipeline copy-free.

Linux window integration uses GTK/X11. Windows and Android window/lifecycle paths
are present in the code, but native validation so far has focused on Linux/X11
with Intel hardware. Native image sharing is currently Linux-specific. Windows
transparent-window selection is rejected by the Vulkan compositor.

Synchronous compositor snapshots use an on-demand GPU copy before presentation.
They support legacy pixel reftests and compositor-readback screenshots when the
surface supports transfer-source usage. Ordinary frames do not make this copy.

The current integration does not expose asynchronous screenshot recording, debug overlays,
non-instanced drawing, storage-table paths, incremental presentation, Metal or
Wayland. Native hardware-video transport is also outside this scope; GTK FFmpeg
paths select software decoding for Vulkan and use the ordinary buffer-image path.

## Development

Add `ac_add_options --enable-webrender-vulkan=naga` to the mozconfig to use the
vendored Naga compiler. The `glslang` choice is also supported and requires the
external shader tools. Selection and tool requirements are described in the
[implementation guide](VulkanWebRenderImplementation.md).

Wrench supports Vulkan `show`, `test_init` and `test_invalidation`, including
application-owned external images. The invalidation harness checks cache metadata
without reading pixels. Native correctness checks must use compositor readback or
observe the actual rendered window: software automation snapshots do not prove
hardware WebRender output.

See the [implementation guide](VulkanWebRenderImplementation.md) for ownership,
synchronization, failure and validation contracts.
