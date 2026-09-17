# Native video DMA-BUF capability probe

`ProbeVideoDmaBuf.cpp` is a standalone Linux Vulkan query tool for the native
video investigation. It matches a Vulkan physical device to a DRM render node
and queries DMA-BUF import with transfer-source usage for an exported modifier.
It reports NV12 and separate R8/RG8 layer candidates, including modifier memory
plane counts and dedicated-allocation requirements.

A successful query is only a capability prerequisite. It does not establish
that a particular decoder allocation can be bound, that aliasing is legal, or
that synchronization, copying and decoder-buffer reuse work. The tool does not
create a logical device, import memory or submit GPU work. Exit 0 means queries
completed, including unsupported format results; exit 1 means invalid input or
an API error, and exit 2 means no matching device or missing required extensions.

From the source root, build using the in-tree Vulkan headers and system loader:

```sh
mkdir -p artifacts/native-video/v0
c++ -std=c++17 -Wall -Wextra -Werror \
  -I third_party/khronos/vulkan-headers/include \
  gfx/webrender_bindings/tests/ProbeVideoDmaBuf.cpp \
  -Wl,-l:libvulkan.so.1 \
  -o artifacts/native-video/v0/probe-video-dmabuf
```

Pass the actual render node, modifier, and even allocation dimensions. For the
initial Intel iHD VP9 export:

```sh
artifacts/native-video/v0/probe-video-dmabuf \
  /dev/dri/renderD128 0x0100000000000002 256 128
```

Select an ICD through the loader's normal environment when needed. Device
matching still uses the render-node major/minor numbers, rather than a device
name or enumeration index. An environment without access to the DRM render node
cannot provide native hardware coverage.

## Initial V0 observations

Investigation completed against `7931804dbb0` on 2026-09-17. The probe built
with the command above and ran successfully. V0 establishes the initial import
strategy and control measurements; actual binding, GPU copies and ownership
validation belong to V2. No decoder-sharing capability is enabled by this tool.

The host reports Intel Iris Xe Graphics (RPL-P), PCI device `8086:a7a0`, Mesa
26.2.2 and Intel iHD 25.4.5. VA-API VP9 Profile 0 decoding is available. The local
one-second VP9 lossless control clip is 256 by 128, 10 fps, 8-bit limited-range
4:2:0. FFmpeg 6.1.1 decoded it with VA-API and exported DRM PRIME frames using:

```sh
LIBVA_TRACE=artifacts/native-video/v0/va-export \
  ffmpeg -hide_banner -loglevel verbose \
  -hwaccel vaapi -hwaccel_device /dev/dri/renderD128 \
  -hwaccel_output_format vaapi -i artifacts/browser-input-video.webm \
  -vf hwmap=mode=read,format=drm_prime -frames:v 1 \
  -c:v wrapped_avframe -f null - \
  > artifacts/native-video/v0/ffmpeg-export-direct.log 2>&1
```

The clip and raw traces are local investigation artifacts, not checked-in test
dependencies. Use an available VP9 Profile 0 clip to repeat the investigation;
its dimensions, modifier and object sizes may differ. The trace demonstrated
successful `vaSyncSurface` and `vaExportSurfaceHandle` calls, and this topology:

| Item | Observed value |
| --- | --- |
| Surface | NV12, 256 by 128 |
| Memory objects | One, 49,152 bytes |
| Modifier | `0x0100000000000002` (tiled) |
| Y layer | R8, object 0, offset 0, pitch 256 |
| UV layer | GR88, object 0, offset 32,768, pitch 256 |
| Export flags | Read-only, separate layers |

Only array entries below each layer's declared plane count are valid. This
libva trace labels the layer count as a second `num_objects` entry; it must not
be interpreted as another memory-object count. A linear-only importer would
not handle this export. Separate layer FDs must not be treated as independent
allocations merely because the handles differ.

## Query result and initial representation

The probe matched DRM device `226:128`. Modifier import, DMA-BUF external
memory, external memory FDs and foreign queue ownership extensions are present.

| Candidate with transfer-source usage | Result |
| --- | --- |
| NV12, no image creation flags | Importable, two modifier memory planes, no dedicated-only requirement |
| R8 Y layer, alias flag | Format/modifier/usage/handle combination unsupported |
| RG8 UV layer, alias flag | Format/modifier/usage/handle combination unsupported |

The NV12 query reports external-memory features `0x6`, compatible handle types
`0x201`, maximum extent 16,384 by 16,384 by 1, and maximum resource size
17,592,186,044,416 bytes. The loader also warns that support for this platform
with the Xe kernel driver is experimental; the result describes this host and
driver combination only.

Select a single non-disjoint NV12 image backed by the exported object, with
explicit modifier plane offsets/pitches and plane-aspect copies into owned
R8/RG8 textures. Use the verified foreign-producer ownership contract, successful
producer completion, and one lease covering both planes. V2 must validate
memory requirements/binding, allocation bounds, copies and ownership return
against the actual decoded allocation. Successful capability queries alone do
not prove that representation works. Do not substitute independently imported
scalar images or enable decoding before these checks pass.

`copy_dmabuf_planes` currently requires independently allocated single-plane
Vulkan images, matching producer device/driver UUIDs and external ownership.
It rejects aliased allocations. The pinned `texture_from_dmabuf_fd` helper
accepts one stride/offset. Preserve those contracts and implement the new
multi-planar importer through a bounded native Vulkan adapter or an additive
HAL API, as established by the V2 binding audit.

## Browser controls

Three fresh runs used the existing Firefox binary, real X11 display, GPU
renderer process and `remote.screenshot.use_readback=true`. No Firefox build
was run. The local `test_native_video_v0_browser.py` harness verifies the live
renderer with `getWebRenderBackendInfo`, reads decoder identity and acceleration
through `mozRequestDebugInfo`, checks compositor pixels and samples 20 video
frame callbacks. Vulkan Video decoding is disabled in these controls.

| Control | Live backend | Hardware acceleration | Top/bottom compositor RGB |
| --- | --- | --- | --- |
| GL, software decode | OpenGL | false | (133, 68, 28) / (28, 65, 131) |
| Vulkan, software decode | Vulkan (wgpu-hal) | false | (133, 68, 28) / (28, 65, 131) |
| GL, hardware decode | OpenGL | true | (127, 64, 31) / (31, 64, 128) |

All three report `ffvpx video decoder (RDD remote)`; the hardware-acceleration
field distinguishes them. The GL hardware control agrees with its video-to-2D
canvas reference within one channel value. The software GL/Vulkan controls
agree exactly. Preserve separate software/hardware references: their current
pixel difference precedes native Vulkan video transport.

| Control | Sample duration (ms) | Frame counter delta | Dropped counter delta | GPU CPU time (ms) | RDD CPU time (ms) |
| --- | --- | --- | --- | --- | --- |
| GL, software decode | 1821 | 22 | 4 | 130.09 | 17.72 |
| Vulkan, software decode | 1789 | 21 | 4 | 260.50 | 17.93 |
| GL, hardware decode | 1870 | 21 | 3 | 119.11 | 57.38 |

These are short functional baseline samples, with startup/looping effects and
validation enabled, not steady-state performance measurements. Frame-counter
deltas come from `getVideoPlaybackQuality`; all three report zero compositor
drops. The mean callback intervals were 95.77, 94.11 and 98.35 ms respectively.
CPU deltas come from matching process IDs in `ChromeUtils.requestProcInfo`
before/after sampling, excluding newly created processes. Full raw callback
and process snapshots remain in local `artifacts/native-video/v0/` JSON files.

The control clip SHA-256 is
`8a53ae75b535b3a43b3e0d031c68989dfb9548f03f712df617a337403a5c1985`.
The recorded commands use the existing local development harness:

```sh
python3 artifacts/run_browser_gate.py inputs --input-case video \
  --video-baseline --backend gl --software-video --step 8.12a.video-v0-gl-sw
python3 artifacts/run_browser_gate.py inputs --input-case video \
  --video-baseline --backend vulkan --software-video --step 8.12a.video-v0-vk-sw
python3 artifacts/run_browser_gate.py inputs --input-case video \
  --video-baseline --backend gl --step 8.12a.video-v0-gl-hw
```

All three controls passed their pixel, renderer, decoder, callback-progress and
log checks. This baseline does not provide native Vulkan decoder-sharing
coverage, parent-renderer coverage or a long-playback performance result.

## Publication and lifetime changes required next

The current source path is:

1. `FFmpegVideoDecoder::CreateImageVAAPI` exports/synchronizes the decoded
   surface and obtains a `VideoFrameSurface` in the decoder process. Exported
   PRIME topology must be captured before `DMABufSurfaceYUV` flattens layers
   into duplicated per-plane FDs.
2. `VideoFramePool::GetVideoFrameSurface` either retains the FFmpeg frame/context
   via `LockVAAPIData`, or copies under pool pressure. `ShouldCopySurfaceLocked`
   also responds to the hardware zero-copy setting. Both publication routes
   need explicit transport support; support for direct VA-API exports does not
   automatically cover the pressure-copy allocation.
3. `DMABufSurfaceYUV::Serialize` forwards handles, global-reference state and
   color metadata to texture consumers. `ReleaseUnusedVAAPIFrames` uses
   renderer-reference observations to decide when to release the retained
   FFmpeg frame. `FlushFFmpegFrames` invalidates the pool's FFmpeg surface IDs;
   repeated output can detach an in-use surface and allocate a fresh wrapper.
   Preserve these cases while preventing reuse during GPU ownership transfers.
4. `DMABUFTextureHostOGL::PushResourceUpdates` publishes NV12 as R8/RG8 image
   keys sharing an external image ID with channels 0 and 1. The native bridge
   must serve either channel order from one materialized frame publication.
5. `wr_renderer_acquire_hal_image` retains the render texture in a lease, but
   `RenderDMABUFTextureHost::LockHalImage` currently rejects nonzero channels
   and lacks a decoder-frame source. Its YUV `MapPlane` path is unsupported.
   A new frame-level cache/lease is needed; retaining a texture alone does not
   serialize competing GPU imports or protect decoder recycling.

Snapshot/readback consumers and pool shutdown need the same ownership/reuse
audit. Abandonment must prevent reuse without allowing unbounded retained
frames. The existing producer-side decode-error path requests hardware-decoder
fallback when `CreateImageVAAPI` fails; a late compositor failure still needs an
explicit route back to transport invalidation and decoder recovery.

The query structure follows the Khronos documentation for
[modifier image capabilities](https://docs.vulkan.org/refpages/latest/refpages/source/VkPhysicalDeviceImageDrmFormatModifierInfoEXT.html).
The distinction between format planes and modifier memory planes is documented
in [modifier properties](https://docs.vulkan.org/refpages/latest/refpages/source/VkDrmFormatModifierPropertiesEXT.html).
