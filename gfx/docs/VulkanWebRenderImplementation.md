# Vulkan WebRender implementation

The [overview](VulkanWebRenderOverview.md) describes the backend's scope. This
page describes the ownership and observable behavior that changes must preserve.

## Core and native ownership

`Device::new` dispatches `GpuBackendConfig::Vulkan` to `RenderDevice`.
`device::wgpu::Device` owns the core instance, adapter, device and queue.
Ordinary textures, buffers, views, samplers, bind groups, pipelines and commands
are core objects. WR retains semantic resource selection, pipeline keys, format
policy, texture pools and frame-to-submission associations. Native interoperability
does not replace core's ordinary resource tracking or retention.

Construction creates the window surface before adapter selection, then checks
Vulkan 1.1, required formats and actual surface capabilities. Optional features
are enabled only when supported. `MAPPABLE_PRIMARY_BUFFERS` is requested only
on supported integrated/CPU adapters; other adapters retain core staging.
The device-opening callback may enable Linux external-memory/semaphore
extensions. Their presence alone does not authorize a native publication.

`SurfaceWindow` is unsafe: its owner must keep the exact native window/display
valid and unchanged until the last surface reference is released.
`WrVulkanConfig`, `OwnedVulkanConfig` and the Rust bridge carry balanced owner
leases. `WindowSurface` drops the core surface before its native owner. A retained
display supports detached startup and window replacement on the same display;
replacing display identity is rejected.

Before GTK destroys an old XID, `NotifyVulkanXWindowChanged` must acknowledge
retirement. The compositor pauses and waits for render-thread surface retirement,
including initialization work without an established renderer. IPC failure needs
explicit local retirement or confirmed process exit. Replacement preserves the
retained scene without reviving a failed device. Windows and Android startup
remain gated as described in the overview.

### Shared WebGPU native device

The default-off startup preference `gfx.webrender.vulkan.shared-webgpu` requests
sharing when Vulkan WR is selected in the GTK/X11 GPU process. The parent latches
the request for the session; replacement GPU processes inherit it. A requested
preference does not prove successful admission.

WR publishes a retained `NativeContext` for its native Vulkan device and queue.
WebGPU actors can join only an active context, with Vulkan enabled in their
backend policy and matching adapter, features and limits. Unsupported admission
uses independent devices. Actors created before WR publishes its context remain
independent; enabling the request does not migrate existing devices.

Clients share native handles, not core resource identities, submission indices
or callbacks. Each client keeps its own validated feature policy and resource
lifetime. WebGPU retains strict compliance and timestamp normalization; WR's
shader-passthrough feature does not grant content that feature. Retained ownership
keeps the native device alive when another renderer or client closes.

HAL serializes shared submissions, presentation and idle operations with the
same queue lock, including WR native-image retirement. Logical client destruction
and validation errors remain isolated. Actual native device loss invalidates the
context and stops further sharing admission. A failed context cannot be replaced
while retained owners remain; callers retain the independent-device fallback.

This sharing does not hand textures between core devices or enable native canvas
publication. Cached WR diagnostics and screenshots identify a renderer, not shared
native handles or producer transport; sharing needs separate admission and handle
checks. No performance result follows from using the same native device.

## Trusted shader artifacts

`webrender_build` defines the catalog. `webrender_shader_build` compiles and links
each vertex/fragment variant with glslang for Vulkan 1.1. SPIRV-Tools validates
the artifacts before and after dense binding remapping. The resulting words are
shipped unchanged, without a Naga SPIR-V round trip.

Reflection checks entry points, audited capabilities, 32-bit interfaces,
descriptor set zero, projection/storage layouts, vertex declarations, fragment
outputs and matching stage locations/interpolation. Sample interpolation and
unsupported requirements fail the build. Runtime checks cover reflected vertex
descriptors, device varying limits and storage-stage requirements; core validates
explicit layouts, bindings, resource usages and commands. Optional blending and
format support remain checked against the actual device.

`prepare_shader` uses unsafe `create_shader_module_passthrough`, requiring
`PASSTHROUGH_SHADERS`. This skips Naga shader validation and runtime bounds/index/
loop instrumentation. The safety contract is trusted generated shaders and
WR-owned draw data. Texture-table records, vertex strides and bounded shader
indices must remain consistent; `spirv-val` does not prove runtime index safety.
Changes to those contracts need corresponding interface and pixel checks.
Content WebGPU shader validation is unchanged.

Tool lookup uses `GLSLANG_VALIDATOR`, `SPIRV_VAL`, `SPIRV_DIS`, configured fetches,
then PATH. glslang is pinned at 15.1.0; the validator must be SPIRV-Tools 2026.1
or newer. These are host tools, also when cross-compiling. Standalone defaults
are tool-free; explicit `glslang` or Vulkan features enable the compiler.
The Gecko-only `gecko-core-shader-tests` feature additionally checks the actual
vendored core timestamp WGSL. Its SSA-composite fix requires rebuilding embedded
shader code; a stale binary or a filtered validation message is not evidence of
valid artifacts.

## Uploads, draws and readback

CPU texture uploads write directly into fresh mapped core `MAP_WRITE | COPY_SRC`
staging, then unmap before encoding a buffer-to-texture copy. Row packing and
channel conversion write into the same mapping. Matching BGRA8 data uses one
CPU copy and one GPU copy. Core retains staging through submission; upload
allocations are not pooled. Immediate uploads avoid an additional CPU PBO copy.

External CPU buffers can be overwritten at unlock. RGB and strided YUV bytes
must therefore reach owned staging before that callback. Buffer writes obey
four-byte alignment, and encoder texture copies use 256-byte row pitches.
Producer offsets/strides are validated separately. Core initialization must
preserve partial updates, mip contents and atlas neighbors, including when
recordings are discarded.

Deferred passes preserve final depth-store decisions and ordering: uploads,
copies, blits and readback flush preceding pass work. Instance slices go directly
into fresh immutable mapped buffers, with checked sizing and zero padding.
Projection buffers remain shared until their transform changes. Atlas subrect
clears preserve neighboring content; attachment-feedback operations use scratch
storage instead of sampling an aliased active attachment.

Readback uses core copies and completion/mapping callbacks, strips row padding
and preserves row order and RGBA/BGRA conversion. Failure leaves caller output
unchanged. Mapping, submission completion and successful presentation are
distinct events.

`capture_external` copies `COPY_SRC`-capable images directly. Sampling-only
`RESOURCE` imports instead use their actual view to blit into owned RGBA8 storage,
then read that target. Normal/opaque component swizzles are preserved. Capture
does not add unsupported source usages or bypass acquisition and native return.

## Presentation, completion and recovery

`Surface` wraps core configure/acquire/present/discard. It negotiates a supported
renderable RGBA/BGRA UNORM format, sRGB color space, alpha mode and present mode.
The acquired texture is the final target; references are released before present
or reconfiguration. Transfer-source usage is requested for snapshots when supported.

| Condition | Behavior |
| --- | --- |
| Timeout | Retry |
| Occluded, paused, detached or zero extent | Skip presentation; resource work can continue |
| Outdated | Reconfigure and retry |
| Lost surface | Recreate the surface and retry |
| Suboptimal | Present, then reconfigure |
| Native/document extent mismatch | Report `SizeMismatch`; update view and target together |
| Validation/device failure | Fail the renderer and enter recovery |

`Renderer::prepare_readback` arms a request after resource updates. The next
actual GPU frame consumes it and copies the surface into an owned snapshot before
present. Resource-only work does not consume it. Ordinary frames do not refresh
the snapshot. Failed/skipped presentation and replacement invalidate readiness;
a consumed request must be rearmed for retry. Readback failure propagates through
`wr_renderer_readback` and Gecko's GetSnapshot path instead of returning stale bytes.

Core submission indices back `GpuSubmissionStatus`, including internal queue
work associated with presentation. `VulkanFrameTracker` associates Gecko frame
IDs with those indices and retires them only after completion. Idle timer polling
progresses pending frames without an unconditional frame-end wait.

Uncaptured-error/device-loss callbacks record failure centrally. Resource updates,
submission, surface operations and completion observe it. Resource-only and
idle-completion errors propagate through `wr_renderer_update` to Gecko recovery;
they must not wait for another visible frame. A returned submission index alone
does not establish a successful submit.

## Native-image admission and return

`ExternalTextureRegistry` binds admitted allocations/views to the actual core
device. It checks identity, dimensions, format/modifier/usage, allocation layout
and FDs. Aliases share access state; an opaque alpha-ONE view is not an independent
publication. The producer must fully initialize the image, publish a genuine
ready signal and keep every alias immutable until its required return condition.

The first raw acquire completes before the image is wrapped as initialized core
storage. Until then the handler reports `Pending`, withholds the rendered epoch
and schedules progress. Retry must perform a real render with the document's
original present/offscreen intent, even after update-only notifications.
Native work orders raw acquire, core sampling and raw release. The core image
is transitioned to the agreed `RESOURCE` state before ownership release.

The GTK GL route uses `SharedSurface_DMABUF`, serialized `DMABufSurfaceRGBA`,
`RenderDMABUFTextureHost` and the registry. Admission requires the actual render
node, supported RGB layout and genuine EGL native-fence support. Factory clearing
preserves caller GL state. `FenceSet` flushes before duplicating the fence FD;
publication does not use `glFinish` as a failure fallback.

Shared DMA-BUF state checks generation and access atomically. Serialization fails
while writing. A generation-N descriptor cannot acquire during writing or after
publication N+1. Unsupported factory admission retains the Basic/CPU-buffer path.
This protocol adds no sandbox-policy exemption.

| Return state | Producer obligation |
| --- | --- |
| `Pending` | Keep the borrow/pin; return is not yet safe |
| `Submitted(value)` | Vulkan EXTERNAL producer waits for this timeline value before reuse |
| `Complete` | Healthy GL FOREIGN release completed; producer lock can reopen |
| `Abandoned` | Borrow is permanently poisoned; no reuse is authorized |

A queued GL release, frame counter, core callback or host destruction cannot
substitute for actual FOREIGN completion. Healthy return retains the shared lock
and global-reference pin until completion. An aborted borrow may drop its C++
reference while access remains poisoned; unlock must never reopen it.

The render-thread receipt queue progresses without new frames. Migration and
GL/SWGL fallback retain the old Vulkan state until pending release callbacks
finish. Uncertain raw work is drained or quarantined by native retirement.
These obligations concern interop allocations, raw commands and synchronization,
not ordinary core resource retention.

WebGPU keeps a separate core device even when its native Vulkan device is shared,
and uses CPU canvas presentation fallback. Generic EXTERNAL consumers and test
publishers are groundwork for later native WebGPU canvas integration, not an
implemented browser producer. Native browser GL transport likewise needs
correlated publication/acquire/return evidence beyond fixture success or correct
compositor pixels.

## Cached backend diagnostics

At `WebRenderAPI` construction, `wr_renderer_get_backend_info` reads actual
renderer facts. The API records the texture limit and owning Parent/GPU process;
SWGL and OpenGL ES use actual compositor/context overrides. `Clone` copies the
immutable JSON cache and per-session backend-presentation value.

`PWebRenderBridge::GetBackendInfo` and privileged
`nsIDOMWindowUtils.getWebRenderBackendInfo()` return that cache without creating a
compositor. Missing, closed or uninitialized APIs report `NS_ERROR_NOT_AVAILABLE`.
The object contains `backend`, `renderer`, `driver`, `maxTextureSize` and `process`,
with no PID or native handles. `Troubleshoot.snapshot()` optionally exposes it as
graphics `webRenderBackend`; `about:support` displays it. The no-initialization
contract applies to this query, not every other operation in Troubleshoot.

## Development checks

From `gfx/wr`, run host checks and build Wrench with the shader tools configured:

```sh
cargo test -p webrender_shader_build --lib
cargo test -p webrender_shader_build --features glslang --lib
cargo test -p webrender_shader_build --features gecko-core-shader-tests --lib
WR_SHADER_COMPILER=glslang cargo build -p wrench --features vulkan
```

The core-shader feature requires Gecko's vendor tree. From `gfx/wr/wrench`, the
default target directory gives these commands (adjust for `CARGO_TARGET_DIR`):

```sh
../target/debug/wrench --vulkan --vulkan-validation test_init
../target/debug/wrench --vulkan --vulkan-validation test_invalidation
../target/debug/wrench --vulkan --vulkan-validation show reftests/clip/clip-between-picclip-and-lca.yaml
```

Vulkan Wrench requires native X11 and conflicts with `--headless`. Invalidation
checks metadata and does not replace pixel checks. From the Firefox object
directory, `./dist/bin/gfxtest vulkan --webrender` checks startup capability;
inspect `VULKAN_WEBRENDER` and diagnostic fields, not just the exit status.

Ignored Rust hardware fixtures require their stated capabilities and Khronos
synchronization validation. For example, from `gfx/wr`:

```sh
cargo test -p webrender --features vulkan,capture,debugger --lib \
  vulkan_renderer_stages_external_rgb_and_video_bytes_before_unlock \
  -- --ignored --test-threads=1 --nocapture
cargo test -p webrender --features vulkan,capture,debugger --lib \
  native_surface_direct_output_discard_resize_and_snapshot \
  -- --ignored --test-threads=1 --nocapture
```

Native-image fixtures cover EXTERNAL imports, aliases, capture, Pending epochs,
idle return, migration and logical loss. The ignored
`vulkan_renderer_foreign_gl_reuses_only_after_complete` additionally needs a
genuine GL producer socket/fence harness (`WR_FOREIGN_SOCKET_FD`,
`WR_FOREIGN_FAILURE`); its test filter alone is not a self-contained publisher.

The C++ FOREIGN fixture exercises the real factory, serialized/imported host,
renderer sampling/capture and return queue. Select the real hardware GL driver
and audited layer described below. From the repository root (replace
`/dev/dri/renderD128` with the matching device's render node):

```sh
MOZ_DRM_DEVICE=/dev/dri/renderD128 \
  WR_NATIVE_IMAGE_TEST=1 WR_GL_FOREIGN_EXPECT_NATIVE=1 \
  ./mach gtest 'VulkanForeignImages.*'
```

The expectation must match the device: Intel has native-positive coverage;
`WR_GL_FOREIGN_EXPECT_NATIVE=0` covers NVIDIA negative admission/Basic fallback
where renderable linear GBM allocation is unavailable. The four cases cover
completed reuse, old-device migration, fallback before release notification and
abandonment. These in-process fixtures do not establish normal-browser selection
or cross-process publication. The Gecko C++ positive cases use real EGL native
fences. The isolated Intel Rust socket fixture uses `glFinish` plus real sync
files; that fixture does not establish deliberately unsignaled producer-fence
coverage.

Native validation used Khronos layer 1.4.341 with synchronization validation.
Select the audited layer explicitly with `VK_LAYER_PATH`,
`VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation` and `VK_VALIDATION_VALIDATE_SYNC=1`.
Verify the loaded library/version and retain raw VUID/SYNC-HAZARD output. Older
system-layer import failures do not establish their driver/layer root cause.

Diagnostics/schema checks can run headless:

```sh
./mach mochitest --flavor browser --headless \
  toolkit/content/tests/browser/browser_aboutsupport_webrender.js \
  toolkit/modules/tests/browser/browser_Troubleshoot.js
```

Headless checks exercise software composition. Real-display runs must verify
the created backend; requesting Vulkan does not prove selection. Browser pixel
checks also need the [capture boundaries](DebuggingWebRenderScreenshots.md).
Logical device-loss fixtures do not establish physical GPU-reset recovery or
performance. Keep source/binary/tool identities and failed evidence with measured
results rather than inferring broader coverage from these commands.
