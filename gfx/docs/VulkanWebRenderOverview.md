# Vulkan WebRender

WebRender has an experimental, opt-in Vulkan backend beneath its shared
renderer. Scene building, batching, texture caches and render tasks remain
shared with OpenGL. Ordinary GPU resources and commands use Firefox's pinned
`wgpu-core`, which owns validation, initialization, resource transitions,
submitted-resource retention and submission progress.

## Enabling the backend

Build Firefox with `ac_add_options --enable-webrender-vulkan=glslang` in the
mozconfig. Set the startup preference `gfx.webrender.vulkan` to `true` and restart.
It defaults to `false`. GTK/X11 hardware WebRender and a successful Vulkan
startup probe are required; the preference alone does not identify the backend
that actually started.

Shader builds use glslang 15.1.0 and SPIRV-Tools 2026.1 or newer. Trusted built-in
shaders use core SPIR-V passthrough after host validation and WR interface
reflection. The device must support `PASSTHROUGH_SHADERS`. Standalone default
host tests do not require external shader tools.

`about:support` displays the actual WebRender backend, renderer, driver, texture
limit and owning process. Privileged callers can obtain the same cached facts
with `JSON.parse(await window.windowUtils.getWebRenderBackendInfo())`.
The query reads facts captured when that window's API was created; it does not
initialize a renderer. `Troubleshoot.snapshot()` includes them in the optional
graphics `webRenderBackend` object when a live initialized renderer is available.

## Architecture

| Layer | Responsibility |
| --- | --- |
| Gecko and `RenderCompositorVulkan` | Selection, native-window ownership, visibility, frame IDs, completion polling and recovery |
| Shared `Renderer` and `Device` | Scene/resource updates, render tasks, draw semantics and backend dispatch |
| Core-backed `RenderDevice` | Translate WR resources, reflected interfaces, draw state and ordered passes to core |
| `wgpu-core` | Resource/command validation, initialization, transitions, retention, surfaces and submission progress |
| Native interoperability boundary | Device opening, admitted external images, queue-family ownership and producer synchronization |

The backend lives under `gfx/wr/webrender/src/device/wgpu/`. WebRender owns its
device and queue. Shared WebGPU devices and native WebGPU canvas publication
are not implemented here; WebGPU canvas presentation retains its CPU fallback.
The native-image consumers and test publishers do not themselves activate a
browser WebGPU producer.

Final composition renders directly into a core-acquired surface texture.
Intermediate picture caches, blits and uploads remain normal rendering work.
An explicit snapshot request captures only the next actual GPU frame;
ordinary presentation does not refresh the snapshot.

## Rendering and producer paths

CPU RGB and strided YUV bytes are copied into mapped core staging before the
producer's unlock callback. Matching BGRA8 images use one CPU copy and one
buffer-to-texture GPU copy. Staging allocations are currently per upload.
Deferred passes preserve atlas contents, per-draw values and ordering across
uploads, copies, blits and readback. Streaming instances use immutable mapped
buffers; supported integrated/CPU adapters can avoid core's extra GPU copy.

Linux native images use a separate admission and ownership protocol. Vulkan
publishers use `EXTERNAL` transfers and timeline synchronization. GL publishers
use `FOREIGN` transfers and genuine EGL native-fence sync files. The GTK GL path
checks device, format and modifier support, and uses a shared generation/access
lock to reject stale publications. Unsupported admission uses the existing
Basic/CPU-buffer fallback. Video also retains its CPU fallback.

Core submission indices drive bounded in-flight work and completion polling.
Resource-only updates and idle polling can report renderer failure without a
new visible frame, allowing Gecko's existing recovery path to run.

## Validation and limits

Maintained checks cover shader artifacts, resources, partial initialization,
uploads, YUV, passes, capture, X11 presentation and replacement, native-image
ownership and diagnostics. Wrench supports native X11 Vulkan rendering and
validation. Ignored hardware tests require their stated device capabilities.

GL FOREIGN fixtures have native-positive Intel coverage. NVIDIA factory checks
cover negative admission and Basic fallback when renderable linear GBM allocation
is unavailable; they do not establish native-positive NVIDIA support. Fixtures
and correct browser pixels do not establish native browser publication without
correlated producer, compositor acquisition and return evidence.

Windows Vulkan startup is gated off because native HWND lifetime is not pinned
and DXGI CPU video staging requires the D3D11 software compositor. Android
remains on OpenGL for SurfaceTexture video. Wayland browser selection, native
layer composition and incremental presentation are outside established coverage.
There is no physical GPU-reset or performance acceptance claim.

See the [implementation guide](VulkanWebRenderImplementation.md) for ownership
contracts and test commands, and the
[screenshot guide](DebuggingWebRenderScreenshots.md) for capture limitations.
