# Vulkan WebRender

WebRender has an opt-in Vulkan backend beneath its shared renderer. Scene
building, batching, texture-cache management, render tasks and composition use the
same renderer as OpenGL. Ordinary GPU resources and commands use Firefox's pinned
`wgpu-core`; core owns validation, initialization, resource transitions, submitted
resource lifetime and submission progress.

## Enabling the backend

Build Firefox with `ac_add_options --enable-webrender-vulkan=glslang` in the
mozconfig. Set the startup preference `gfx.webrender.vulkan` to `true` and restart.
The preference defaults to `false`. GTK/X11 hardware WebRender and a successful
Vulkan startup probe are required; a requested preference alone does not identify
the backend that actually started.

The build uses glslang 15.1.0 and SPIRV-Tools 2026.1 or newer. Trusted built-in
shaders use core SPIR-V passthrough after build-time validation and WR interface
reflection; the selected device must support `PASSTHROUGH_SHADERS`. This path
skips Naga shader validation and runtime instrumentation. Standalone default host
tests remain free of external shader-tool requirements; explicit Vulkan or
`glslang` features opt into those tools.

`about:support` displays the actual WebRender backend, renderer, driver, texture
limit and owning process. Privileged callers can obtain the same cached facts
with `JSON.parse(await window.windowUtils.getWebRenderBackendInfo())`.
The graphics object returned by `await Troubleshoot.snapshot()` contains
`webRenderBackend` when an initialized window renderer is available. The query
reads facts captured when that window's API was created; it does not initialize
a renderer or inspect GPU resources to answer a diagnostics request.

## Architecture

| Layer | Responsibility |
| --- | --- |
| Gecko and `RenderCompositorVulkan` | Backend selection, native-window owners, visibility, frame IDs, completion polling and recovery |
| Shared `Renderer` and `Device` | Scene/resource updates, render tasks, draw semantics and backend dispatch |
| Core-backed `RenderDevice` | Translate WR resources, reflected interfaces, draw state and ordered passes to core |
| `wgpu-core` | Resource/command validation, initialization, transitions, submitted resource retention, surfaces and submission progress |
| Explicit native boundary | Device construction, admitted external images, queue-family ownership, producer synchronization and native-only retirement |

The backend lives under `gfx/wr/webrender/src/device/wgpu/`. WebRender has its
own device and queue; a WebGPU canvas does not share them. Direct HAL/Vulkan
access is confined to the native boundary. It does not replace core's ordinary
resource tracking or submitted-resource retention.

Final composition renders directly into a core-acquired surface texture.
Intermediate picture caches, blits and uploads remain normal rendering work.
An explicit window snapshot request arms only the next actual GPU frame. That
frame adds an on-demand copy before presentation when the surface supports
transfer-source usage; ordinary presentation does not refresh a capture.
External-image capture can also sample a RESOURCE-only view into owned core
scratch storage before readback.

## Rendering and producer paths

Core textures, buffers, views, samplers, bind groups and pipelines implement
ordinary rendering. WR retains semantic texture pools and reuse caches that
select resources and avoid allocations. Optional features remain conditional on
adapter and format support; the backend does not raise its Vulkan 1.1 minimum
to enable every optional pipeline.

Uploads preserve source strides, channel conversion, partial texture contents
and mip selection. CPU RGB and strided YUV bytes are copied directly into mapped
core MAP_WRITE | COPY_SRC staging before the producer's unlock callback. Matching
BGRA8 images use one CPU copy and one buffer-to-texture GPU copy. Staging is
unmapped before encoding and core retains it through completion; allocations are
currently per upload rather than pooled. Deferred passes preserve atlas contents,
per-draw uniform values and ordering across uploads, copies, blits and readback.

Streaming instance data goes straight from the draw's source slice into a fresh
immutable mapped core buffer, without an intermediate CPU vector. Supported
integrated or CPU adapters use optional `MAPPABLE_PRIMARY_BUFFERS` to avoid core's
staging-to-primary GPU copy; discrete or unsupported adapters retain that copy.
Projection buffers are shared until the transform changes. Continuous Stretch
gradient borders evaluate the mapped source coordinates directly, removing their
nine source render tasks; discontinuities and unsupported repeat/offset cases
retain the filtered-source path. Border antialiasing is unchanged.

Linux native images have a separate, checked admission and ownership protocol.
Vulkan publishers use `EXTERNAL` queue-family transfers and timeline waits; GL
publishers use genuine EGL native-fence sync files and `FOREIGN` transfers.
The GTK GL path requires matching real device/format/modifier capabilities and
an atomic shared generation/access lock. A stale descriptor cannot acquire a
reused publication. Failed or unsupported admission uses the established CPU
fallback. Admitted ordinary WebGPU canvases use one GPU export copy into an
immutable DMA-BUF publication, followed by WebRender sampling, with no
presentation CPU readback, mapping or pixel copy. Export requires matching
features, device/driver identity, format/modifier/usage and opacity capabilities.
One-shot allocations are bounded to six exports and 256 MiB; unsupported or
budget-limited frames retain CPU readback/staging fallback. Opaque presentation
preserves source alpha, using an alpha-ONE native view or forcing destination
alpha during the existing CPU copy. Explicit snapshots still require readback.
Video retains its CPU fallback.

Core submission indices drive bounded in-flight work and completion polling.
Resource-only updates and idle polling can report a renderer failure without a
new window frame, allowing Gecko's existing renderer-recovery path to run.

## Validation and limits

Maintained checks cover shader artifacts, core resources, partial initialization,
uploads, YUV planes, render passes, capture, surface statuses, X11 presentation,
native-window replacement, native-image return and backend diagnostics. Wrench
supports `--vulkan` and `--vulkan-validation` for native X11 rendering; its
invalidation test checks cache metadata and is complemented by pixel checks.

Actual browser checks include NVIDIA Vulkan, OpenGL with Mesa llvmpipe, SWGL,
and Intel Iris Xe Vulkan. The final Intel normal-browser probe checked the
original mixed Canvas2D/WebGL/WebGPU-readback/video page through compositor
GetSnapshot before additional producer readbacks, direct X11 pixels, and cached
API/Troubleshoot/about:support agreement, with synchronization validation and a
normal exit. These observations do not identify the browser's GL publication
transport: native browser `FOREIGN` publication remains unproven without directly
correlated producer/acquire/return evidence. Dedicated native fixtures establish
the narrower native protocol and renderer contracts.

Separate physical Intel browser probes validate admitted native WebGPU canvas
publication and forced CPU fallback, including RGBA/BGRA queue writes without a
user submit, opaque/premultiplied pixels, source reads, snapshots and retained
output after producer destruction. Correlated publication/acquire/return evidence
identifies the native path; explicit observer readbacks remain separate from
presentation transport.

Windows Vulkan startup is intentionally gated off. Retaining a widget does not
pin its HWND against independent destruction, and DXGI CPU video staging needs
the D3D11 software compositor. Android remains on OpenGL for SurfaceTexture
video. Windows Rust compile coverage does not establish Windows C++ or runtime
support; neither a preference nor a force flag bypasses these platform gates.
Wayland browser selection, native layer composition and incremental presentation
are outside the established coverage.

There is no physical GPU-reset, performance or live actor-notification injection
claim. Hardware readback requires a real display;
headless Firefox uses software composition. See the
[implementation guide](VulkanWebRenderImplementation.md) for ownership and
maintained test commands, and the
[screenshot guide](DebuggingWebRenderScreenshots.md) for capture limitations.
