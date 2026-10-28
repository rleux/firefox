# Vulkan WebRender

WebRender has an opt-in Vulkan backend beneath its shared renderer. Scene building,
batching, render tasks, texture-cache management and composition use the same
renderer as OpenGL. The Vulkan device implementation records native work through
Firefox's pinned `wgpu-hal` dependency.

The startup preference `gfx.webrender.vulkan` defaults to `false`. It requires a
build with Vulkan enabled, hardware WebRender and a supported window system.
Restart Firefox after changing it. `about:support` reports the API and device that
actually started, including software fallback. Rendering requires Vulkan 1.1
and suitable device and surface capabilities.

## Architecture

| Layer | Responsibility |
| --- | --- |
| Gecko and `RenderCompositorVulkan` | Window ownership, visibility, frame completion and recovery |
| Shared `Renderer` and `Device` | Scene/resource updates, render tasks, draw state and backend selection |
| HAL `RenderDevice` | Shared resources, pipelines, pass recording, bindings and submissions |
| Native Vulkan integration | Device creation, shader input and external memory/synchronization |
| `wgpu-hal` dynamic interfaces | Device, queue, resource and presentation operations |

`Device::new` constructs the backend from `GpuBackendConfig`. Ordinary WebRender
owns its Vulkan device independently of WebGPU. With
`gfx.webrender.vulkan.unified-webgpu=true` at startup and compatible GPU-process
Vulkan consumers, `ProcessOwner` retains one native `VkDevice` for WebRender
graphics and private WebGPU actors. Actors keep independent logical Core devices;
compute uses a separate native queue, while validated graphics work runs through
WebRender's graphics endpoint. Native image producers use explicit publication
and synchronization protocols.

GPU commands are batched with explicit synchronization and resource retention
until completion. Upload buffers and temporary textures are pooled; pipelines
and resource bindings are cached. Draw instances use pooled uploads for each
draw. Texture blits and mipmap generation run on the GPU for supported formats.
Surfaces can be detached and replaced while retaining the device and resources.
Hidden X11 windows skip presentation while resource updates continue. Permanent
GPU failures enter Firefox's existing renderer recovery path.

Final composition renders directly into an acquired swapchain image. Picture
caches and other intermediate render targets remain part of ordinary WebRender
rendering. Presentation does not require a full-frame copy from a retained output
texture.

## Scope

The implementation covers ordinary rendering resources, text, images, clips,
gradients, filters, YUV sampling and native-window presentation. CPU images and
ordinary WebGPU canvases use the upload/readback path. Linux GL producers can
publish compatible linear RGB DMA-BUF images for direct sampling; their producer-side
work remains. Opt-in native WebGPU canvases use process-local native image
publications and bounded storage reuse, without DMA-BUF or CPU final-texture
import. Explicit snapshots still use readback.

Vulkan startup supports Linux/GTK/X11 and Windows. Android window/lifecycle paths
are present in the code, but Android keeps accelerated OpenGL until Vulkan can
consume its native video images; the startup preference does not enable it there.
X11 support and alpha visuals are probed before activation; unsupported window
transparency can require software rendering.
Native validation so far has focused on Linux/X11 with Intel hardware. Native
image sharing requires additional capabilities and is limited to Linux RGB
images. Windows transparent-window selection
is rejected by the Vulkan compositor. Windows and Android still require native
build and runtime validation.

Synchronous compositor snapshots use an on-demand GPU copy before presentation.
They support legacy pixel reftests and compositor-readback screenshots when the
surface supports transfer-source usage. Ordinary frames do not make this copy.

The current integration does not expose native layer composition, incremental
presentation, asynchronous screenshots, composition recording, runtime shader
overrides, debug overlays, non-instanced drawing, multisampling, storage-table
paths, Metal or Wayland. Native hardware-video transport is also outside this scope; GTK FFmpeg
paths select software decoding for Vulkan and use the ordinary buffer-image path.

## Development

Existing GLSL shaders are compiled into embedded SPIR-V with glslang at build
time. Shader tools run on the build host and are not required by the installed
browser.
Vendored wgpu includes local native integration changes; Naga remains unmodified.

Add `ac_add_options --enable-webrender-vulkan=glslang` to the mozconfig. Compilation
requires the external shader tools. Selection and tool requirements are described in the
[implementation guide](VulkanWebRenderImplementation.md).

Wrench supports Vulkan `show`, `test_init` and `test_invalidation`, including
application-owned external images. The invalidation harness checks cache metadata
without reading pixels. Native correctness checks must use compositor readback or
observe the actual rendered window: software automation snapshots do not prove
hardware WebRender output. Broader Vulkan harness support remains incomplete.

Temporarily unavailable external images defer rendering and trigger retries.

See the [implementation guide](VulkanWebRenderImplementation.md) for detailed
ownership, synchronization, failure and validation contracts.
