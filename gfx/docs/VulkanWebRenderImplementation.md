# Vulkan WebRender implementation

The [overview](VulkanWebRenderOverview.md) describes the supported scope. This page
explains the boundaries and invariants that changes to the backend must preserve.

## Selection and construction

`--enable-webrender-vulkan=naga` or `--enable-webrender-vulkan=glslang` enables the
backend at build time. The configure option accepts Linux, Windows and Android
targets. `gfxPlatform::InitWebRenderConfig` additionally requires hardware
WebRender and the startup preference `gfx.webrender.vulkan`. On GTK it requires
an X11 display.

`RenderCompositorVulkan` supplies an owned native-window configuration to
`wr_window_new`. The Rust binding selects `GpuBackendConfig::Vulkan`; the ordinary
`Device::new` constructs the Vulkan backend. The same `Renderer` continues to
consume scene-builder results, update caches and issue draws. Vulkan window
creation rejects GL/SWGL state and Native, Layer or partial-present compositor
callbacks, because this integration uses the Draw compositor.

Source entry points are `gfxPlatform`, `RenderCompositorVulkan`,
`webrender_bindings::bindings::window_backend_config`, `Device::new` and
`device::vulkan::RenderDevice`. The Vulkan implementation lives under
`gfx/wr/webrender/src/device/vulkan/`.

## Host runtime requirements

These requirements describe the Vulkan WebRender backend in this branch, not
Vulkan video decoding or WebGPU in general. Shader compilers are build-host tools;
an installed Firefox does not need Naga, glslang or SPIRV-Tools on the runtime host.

### Required for window rendering

| Capability | Requirement |
| --- | --- |
| Loader and driver | A working Vulkan loader and ICD accessible to Firefox, with Vulkan 1.1 or newer. Both shader routes target Vulkan 1.1/SPIR-V 1.3; the HAL's ability to enumerate older devices does not make them suitable for these shaders. |
| Instance extensions | `VK_KHR_surface` and the extension for the native window: `VK_KHR_xlib_surface` for GTK/X11, `VK_KHR_win32_surface` for Windows, or `VK_KHR_android_surface` for Android. |
| Device extension | `VK_KHR_swapchain`. |
| Queue | The first queue family must support graphics, as required by the pinned HAL, and must support presentation to the actual window surface. |
| Texture formats | Device initialization checks `R8G8B8A8_UNORM` color-attachment and transfer-source support, and `D32_SFLOAT` depth-attachment and transfer-source support. Individual resource allocations also check their sampling, filtering, copy and attachment usages. |
| Surface | Direct color-attachment rendering to `R8G8B8A8_UNORM` or `B8G8R8A8_UNORM` in the sRGB nonlinear color space, a supported composite-alpha mode, FIFO presentation, and an extent within device limits. |
| Firefox integration | A Vulkan-enabled build, hardware WebRender, `gfx.webrender.vulkan=true`, and a supported native window system. GTK currently requires X11. |

API version and extension names alone are insufficient: format usages, queue
presentation support and surface capabilities must also pass. Vulkan 1.1 includes
the maintenance and storage-buffer functionality for which the HAL would need
`VK_KHR_maintenance1` and `VK_KHR_storage_buffer_storage_class` on Vulkan 1.0.

### Optional capabilities

| Capability | Requirements and behavior when unavailable |
| --- | --- |
| Timeline-based queue completion | Vulkan 1.2 or `VK_KHR_timeline_semaphore`, with the `timelineSemaphore` feature enabled. The HAL uses a fence pool when timeline semaphores are unavailable; ordinary rendering does not require external semaphores. |
| Linux native image sharing | `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`, `VK_EXT_image_drm_format_modifier`, and `VK_KHR_external_semaphore_fd`, plus timeline semaphore support. The renderer additionally requires importable and exportable `OPAQUE_FD` timeline semaphores. Missing capabilities disable native publications, leaving buffer paths where the producer supports them. |
| WebGPU RGB publications | The native-sharing capabilities above, matching device/driver identities, and compatible importable image format, modifier, layout and memory properties. Ownership transfers use `QUEUE_FAMILY_EXTERNAL`; producer and consumer use separate logical devices. |
| GL RGB publications | The native-sharing capabilities plus `VK_EXT_queue_family_foreign` and importable `SYNC_FD` binary semaphores. The current admission path requires linear, single-plane ABGR8888 or ARGB8888 storage and compatible EGL producer support. Ownership transfers use `QUEUE_FAMILY_FOREIGN_EXT`. |
| Synchronous screenshots | Transfer-source usage on the swapchain surface. Unsupported capture fails without disabling ordinary rendering. |
| Rendering features | Dual-source blending, 16-bit normalized texture formats and timestamp-query features are requested only when exposed by the adapter. Their presence is not a baseline device-selection requirement. |
| Diagnostics | `VK_LAYER_KHRONOS_validation` is required only when validation is explicitly requested. The validation-layer version discussed below is not the minimum Vulkan API version. |

The HAL may enable additional supported extensions for optimizations and
workarounds. That enabled-extension list is not a list of mandatory host features.
The sources of these requirements are `Device::new`, `external::open_adapter`,
`surface_config::negotiate`, and the pinned `wgpu-hal` Vulkan adapter code.

### Checking a host

On GTK/X11, a Vulkan-enabled build includes `gfxtest vulkan --webrender`.
Run it on the same display and with the same driver environment as Firefox:

```sh
obj-x86_64-pc-linux-gnu/dist/bin/gfxtest vulkan --webrender
```

Use your build's object directory in place of the example. The probe checks the
loader, native surface, selected adapter's API version, swapchain extension,
format usages and presentation capabilities, and attempts logical-device creation.
It reports `VULKAN_WEBRENDER` followed by `TRUE` or `FALSE`, with an `ERROR` reason
on failure. The process exit status alone does not indicate capability support.
Firefox runs this probe in a separate process before selecting Vulkan on GTK/X11;
a failed, crashed, timed-out or incomplete probe leaves Vulkan disabled even when
the startup preference is enabled. Optional external-image capabilities do not
block ordinary rendering. Resource allocation and surface configuration still
validate their actual usages and extents at runtime. Windows and Android do not
use this GTK probe.

The separate `gfxtest vulkan -p` mode, when built with Vulkan video support,
probes video-decoder capabilities. Its Vulkan 1.3 and video-extension checks do
not establish whether Vulkan WebRender can run. Wrench's Vulkan `test_init`
exercises renderer initialization on a native window; `test_invalidation` and
`show` exercise rendering. Use the commands below, omitting `--validation` when
the validation layer is not installed. Neither a successful video probe nor
`about:support` is an exhaustive test of optional native-image interoperability.

## Recording and resources

`Device` gathers render state before binding a program. Vulkan pipeline selection
must include the relevant shader, vertex layout, blend/depth state and attachment
formats. A draw must not use a pipeline bound for an earlier state. Texture and
sampler bindings are resolved to the actual resources used by the submission.

`RenderDevice` adapts WebRender handles to native textures, buffers and programs.
It records resource changes and draw work through `SubmissionQueue`. Texture
initialization and usage are tracked per mip. Planned transitions are associated
with a recording and become committed only after successful submission; discarded
recordings must not turn unexecuted work into initialized content.

Resources retain their creating device. Submission-owned references keep buffers,
textures, views and other dependencies alive through GPU completion. A returned
upload buffer becomes writable again only when the pool has exclusive ownership.
`BufferPool` serves upload allocations used by the buffer and texture paths;
`TexturePool` retains reusable intermediate targets. Their cache limits do not
describe total live GPU memory or permit reuse of busy resources.

CPU image data is consumed into mapped upload storage. Partial updates preserve
untouched texels, and the backend performs layout conversion when the source and
native formats require it. Moving a CPU-source lifetime notification earlier than
the actual upload would allow its producer to overwrite bytes still in use.
GPU completion is a separate lifetime checkpoint.

Render-pass load/store operations must preserve shared atlases. Work covering a
subregion is not permission to discard the whole allocation. Invalidating a target
also invalidates bindings that could otherwise sample its stale contents. Resource
updates must finish even when no window frame is presented.

## Direct presentation and window lifetime

The swapchain image is the final render target. Surface negotiation requires a
directly renderable format and usage; it does not introduce a presentation blit
when that contract cannot be met. Offscreen caches, texture copies needed by
rendering, and producer export copies have separate purposes.

`RenderResults::present_result` tells the embedder whether presentation succeeded,
needs a retry, is occluded or needs a size update. A retry must be scheduled even
when no new scene arrives. Resize must pair the new target extent with an updated
document view. Wrench flushes its resize scene change before drawing into the new
extent, so the previous viewport cannot overrun a smaller swapchain image.

Native-window owners outlive the surface using their handles. A separate display
owner lets the renderer detach and replace windows while retaining its device.
Android configurations retain their `ANativeWindow`; X11 configurations retain
their display connection independently of the widget. Surface detachment and
pausing drain the relevant work before native resources can be released.

`X11WindowVisibility` distinguishes an unmapped/unviewable or WM-hidden window
from visible or unknown state. Focus and ordinary overlapping windows do not prove
that rendering is unnecessary. Hidden-window rendering can still service caches
and resource work. Restoration requests a redraw.

Incremental presentation and swapchain-image damage histories are not enabled.
The current presentation path does not rely on preserving obscured swapchain
contents between acquisitions.

For synchronous capture, Gecko calls `Renderer::prepare_frame_readback` before
rendering. Vulkan copies the rendered swapchain image to an owned texture before
presentation, only for a requested capture. Readback waits for the copy, preserves
top-down row order, and converts RGBA/BGRA channel order when needed. A subsequent
frame cannot return the previous capture as current pixels.

The first capture request enables transfer-source usage when supported; its absence
does not prevent ordinary rendering. Missing, unsupported or invalid captures return
failure without writing the caller's buffer or poisoning the renderer. Snapshot
success is propagated through Gecko's readback IPC so callers do not consume
failed captures. Asynchronous profiler screenshots and composition recording remain
disabled for Vulkan.

## Completion and failure

`VulkanFrameTracker` associates Gecko frame IDs with submission serials. Completed
frame IDs advance only when the relevant serial has completed. Queues bound
outstanding work; backpressure can wait rather than accumulating unbounded
submissions.

The compositor's completion timer runs while frames or image-return receipts are
pending. A page that stops producing frames must still release resources and
return producer access. Timer polling is not an unconditional frame-end GPU wait.

Device loss, invalid completion state or submission failure stops further use of
the failed renderer and feeds Gecko's recovery path. Retained images are republished
when an `ImageContainer` replaces its client. GPU-video descriptors from an obsolete
ImageBridge are rejected; an unavailable readback fallback is handled as missing
data.

During clean teardown, `RendererOGL` destroys the Rust renderer before detaching its
compositor. This lets completed image reads return ownership normally. The
compositor clears its borrowed renderer pointer before invoking final callbacks.
Uncertain or failed completion abandons ownership rather than granting unsafe reuse.

Device references currently express resource lifetime through `Rc`. A stronger
borrowed-device/recording type model is possible future work; it would need to
account for resources retained asynchronously by submissions and external owners.

## External images

`ExternalTextureRegistry` associates application-visible handles with textures on
one renderer's device. Handles do not authorize access to another device's image.
Removing a name does not invalidate resources already retained by bindings or
submissions.

The C++/Rust integration uses `VulkanImageCapabilities` to admit compatible native
publications, `RenderTextureHost` implementations to acquire them, and image-return
receipts to release access. Admission includes device identity, layout, format,
extent and required synchronization support. An OS handle alone is not sufficient
to make a texture usable. Producers and adapters select buffer paths where available.

| Producer | Acquisition | Return |
| --- | --- | --- |
| CPU buffer images | Validated descriptor and mapped bytes through `LockExternalBuffer` | CPU source can be reused after upload consumption; GPU staging remains retained |
| WebGPU RGB DMA-BUF | Import compatible external memory and wait on the producer timeline | Return a submitted consumer timeline value for producer-side GPU waiting |
| GL RGB DMA-BUF | Acquire shared access, import the publication's EGL `SYNC_FD`, and transfer FOREIGN ownership | Return access only after the GPU read has completed; poison abandoned access |

WebRender and WebGPU do not share a logical device. WebGPU's export step remains
producer work even when WebRender samples the exported image directly. GL retains
its resolve/presentation work. Layout admission is deliberately narrower than all
formats a producer might create.

GL publications carry generation, access and fence metadata. Fence descriptors
remain serializable after a producer-side wait. The imported sync-file semaphore
is a single-use dependency retained through submission retirement. Nested normal
and opaque views share access to the same allocation.

Temporary acquisition contention returns `ExternalImageSource::Pending`. The
renderer defers drawing into cached tiles and retains the necessary queued updates.
The embedder schedules another attempt. Temporary unavailability must not become
blank cached pixels. Image migration to another compatible renderer requires
completion of the previous renderer's reads.

Wrench's `VulkanImages` creates immutable test textures on the renderer's device,
uploads them through the existing buffer pool and publishes registry handles.
The reader and renderer share handler state so later scene updates can add images.
Rectangle-style YAML inputs retain texel UVs on Vulkan 2D textures.

## Shader builds

`webrender_build` supplies the shared shader catalog and feature definitions.
The host-side `webrender_shader_build` crate compiles Vulkan variants, records
reflection/layout information and generates the artifacts consumed by WebRender.
The backend reuses WebRender's GLSL and data layouts.

The compiler is selected at build time. Naga consumes GLSL through the vendored
frontend and emits SPIR-V, including the vendored compatibility fixes required by
WebRender shaders. This path does not require the old SPIR-V-to-Naga translation
dependencies. The glslang path uses `glslangValidator`, `spirv-val` and `spirv-dis`.

For glslang, tool lookup uses `GLSLANG_VALIDATOR`, `SPIRV_VAL` and `SPIRV_DIS`, then
`MOZ_FETCHES_DIR/shader-tools/bin` when configured, otherwise `PATH`. The pinned
toolchain recipe is `taskcluster/scripts/misc/build-shader-tools.sh`. These are
host tools, including when targeting Windows or Android.

Firefox selects the compiler with `--enable-webrender-vulkan`. Standalone Cargo
builds can set `WR_SHADER_COMPILER` explicitly and must enable the corresponding
features. An unavailable compiler is an error; there is no silent fallback to a
different shader route.

## Development and validation

For a standalone Naga build, run from `gfx/wr`:

```sh
WR_SHADER_COMPILER=naga cargo build -j8 -p wrench --features vulkan-naga
```

For an interactive scene, run from `gfx/wr/wrench`:

```sh
../target/debug/wrench --backend vulkan --validation show reftests/clip/clip-between-picclip-and-lca.yaml
```

`--adapter` selects an adapter by name substring. `--validation` requires Vulkan
validation support. The Vulkan `show`, `test_init` and `test_invalidation` commands
require a native window. GL remains the default backend. Vulkan commands that
depend on the deferred readback/capture or performance paths are rejected.

Run the existing invalidation harness from the same directory:

```sh
../target/debug/wrench --backend vulkan --validation test_invalidation
```

In Firefox, `about:support` and the privileged `getWebRenderBackendInfo()` window
query expose the API, renderer/adapter, driver, texture limit and owning process.
The information comes from the renderer that was created, not just its requested
preference. It is cached with `WebRenderAPI` so diagnostics need no render-thread
query or GPU work.

Standard Vulkan layer configuration, such as
`VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation`, can enable browser validation.
Use `VK_LAYER_PATH` for layers outside the loader's normal search directories.
For synchronization checks, use Khronos validation layer 1.4.313 or newer, with
`VK_LAYER_VALIDATE_SYNC=1`. Layer 1.3.275
[does not support timeline-semaphore synchronization validation](https://github.com/KhronosGroup/Vulkan-ValidationLayers/blob/v1.3.275/docs/synchronization_usage.md#known-limitations).
It cannot track the timeline-semaphore waits used by `wgpu-hal` fences and can
report false read-after-write or write-after-write hazards when completed
readback allocations are reused.

Set `VK_LOADER_DEBUG=layer` to verify the loaded layer library, as well as the
selected manifest. With an unpacked package, ensure its library is on
`LD_LIBRARY_PATH` or set the manifest's `library_path` to the extracted library;
selecting a manifest alone can still load the system's older library.
Validation-layer availability and driver support are separate from successful
shader compilation.

Coverage includes native resource/upload regressions, presentation and window
lifetime tests, external buffer/native-image checks, ImageBridge recovery,
diagnostics, and Wrench invalidation checks. Local native validation has focused
on Linux/X11 with Intel hardware. Windows and Android paths still need native
build and runtime validation; this is not an all-platform acceptance result.

For pixel checks, use compositor readback or observe the actual rendered window.
Default automation screenshots can use software `drawSnapshot`. Set
`remote.screenshot.use_readback` to `true` and capture in content scope to exercise
the Vulkan readback path. Headless Firefox does not establish hardware WebRender
output, and readback forces a new frame, so native window captures are still needed
for partial-present and damage-tracking artifacts. See
[WebRender screenshot debugging](DebuggingWebRenderScreenshots.md) for the general
distinction between software snapshots and compositor output.
