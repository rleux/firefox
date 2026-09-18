# Native VA-API video tests

The experimental importer maps one completed, same-device NV12 DMA-BUF as a
mutable Vulkan image. R8 and RG8 views share that image, memory, layout state
and publication lifetime. WebRender samples those views with its YUV shaders;
the importer does not copy the frame into owned plane textures. Explicit
snapshots and test reference readbacks still copy pixels.

The initial subset is even-sized NV12 with linear or Intel Y-tiled memory,
one object and two memory planes. The driver must support the exact modifier,
sampled usage, filtering, view formats and foreign queue ownership. Browser
publication requires the default-off `gfx.webrender.vulkan` preference,
hardware-decoding and zero-copy eligibility, and a successful capability probe.
Restart Firefox after changing the Vulkan preference. Vulkan Video remains
disabled; this path uses VA-API on X11.

`import_vaapi_nv12` requires its caller to establish producer completion,
retain the decoder frame, serialize other ownership transfers and forbid writes
until the release callback permits reuse. Y/UV leases share one publication.
The last lease returns ownership after GPU completion; abandoned publications
cannot acquire new leases and must not be recycled. A cache must not retain
publications indefinitely.

## Standalone build

`ExportVAAPIFrame.cpp` uses the in-tree FFmpeg 6 headers and the system
libavcodec 60/libavutil 58/libva libraries. It decodes the first VP9 IVF frame
in VA-API, synchronizes and exports it, and retains the frame while a child
process runs the Rust tests. CPU readback supplies a test reference only.

From the source root:

```sh
mkdir -p artifacts/native-video/v2
c++ -std=c++17 -Wall -Wextra -Werror \
  -I dom/media/platforms/ffmpeg/ffmpeg60/include -I media/mozva -I widget/gtk \
  gfx/webrender_bindings/tests/ExportVAAPIFrame.cpp \
  -Wl,-l:libavcodec.so.60 -Wl,-l:libavutil.so.58 -Wl,-l:libva.so.2 \
  -o artifacts/native-video/v2/export-vaapi-frame
```

Build standalone WebRender from `gfx/wr` so Cargo uses its source configuration:

```sh
cd gfx/wr
cargo test -p webrender --lib --features hal-naga,hal-linux-dmabuf \
  --release --no-run --offline --target-dir ../../artifacts/native-video/build
```

Reuse a compatible target directory when available. Cargo prints the
`webrender-<hash>` executable used as `TEST_BINARY` below. The host needs the
shader tools required by the HAL build. These commands do not build Firefox.

## Native runs

From the source root, generate a detailed frame with padding:

```sh
ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc2=size=250x130:rate=1 \
  -frames:v 1 -c:v libvpx-vp9 -lossless 1 -pix_fmt yuv420p -f ivf \
  artifacts/native-video/v2/padded.ivf
```

Select a supported hardware ICD and compatible validation-layer directory
through the Vulkan loader environment, then run:

```sh
python3 gfx/webrender_bindings/tests/run_native_video.py \
  --exporter artifacts/native-video/v2/export-vaapi-frame \
  --binary TEST_BINARY --clip artifacts/native-video/v2/padded.ivf \
  --output artifacts/native-video/v2/padded
```

Override the default `/dev/dri/renderD128` using `--render-node` if needed.
Use `WR_HAL_SHADER_INPUT=naga` for translated shaders or `native` for SPIR-V.
The runner checks process status, a nonempty test result and validation
diagnostics, and terminates the fixture process group on timeout. It never
builds anything.

The runner also accepts `--icd`, `--validation-layers` and `--shader-input` to
set those choices explicitly instead of using the caller's environment.

## Browser bridge tests

Build the `browser_hal_image_leases` target with the same Cargo flags, replacing
`--lib` with `--test browser_hal_image_leases`, and pass its printed executable
to the same native runner. The exporter supplies a shared lock handle as well
as the live decoded frame. These tests use callback shims to exercise the Rust
browser bridge with real GPU imports, independently of a Firefox rebuild.

They verify UV-first and duplicate channel requests, one lock per publication,
weak-cache retirement, stale metadata and device rejection, a busy lock without
poisoning, and one completed frame release after rendering. C++ gtests separately
cover the actual shared lock and descriptor-to-HAL conversion. Producer lifetime
tests are described below; the browser acceptance runner exercises capability
negotiation, image readers and fallback together.

Native tests compare decoded Y/UV bytes and rendered pixels with controls,
including nearest/linear modes, cropped visible dimensions and downscaling.
They verify shared image identity, retention while either plane lease is alive,
device mismatch, second-view cleanup and injected submission failure.
Pure layout/release tests use the binary's `nv12_` filter without the fixture.

The kernel can round the backing allocation above the decoder-reported size.
That padding is accepted, but plane bounds and Vulkan memory requirements must
fit the reported size, and the report must fit the actual backing object.

Separate-object NV12, P010, cross-device import and protected frames remain
unsupported by this importer. These standalone tests establish HAL behavior;
the browser tests below cover decoder publication and playback separately.

## Producer lifetime tests

`VAAPIFramePool.*` exercises a separate native pool with counted FFmpeg frame
and context references. Publications retain both references until the caller,
image readers and shared access lock permit retirement. Repeated output of a
live allocation shares its immutable publication, including across flush;
changed layout or color metadata is rejected.

Retirement atomically changes the shared lock from idle to a terminal state,
preventing late consumers from acquiring a retired publication. Busy access
retains the frame. Abandonment stops further publication and retains affected
frames until pool shutdown, which follows codec shutdown. The native pool never
uses the legacy pressure-copy path: its limit is three quarters of a fixed
decoder pool, clamped to 1–32 frames, or 32 frames for a dynamic pool. Exhaustion
fails publication and stops that pool.

The tests cover retained callers and images, repeated output across flush, busy
access, pressure, abandonment, metadata changes and partial reference failure.
`DMABufSurface.VAAPIExportCleanupClosesUnreferencedObjects` verifies that export
cleanup closes objects even when no layer references them.

The internal `UseWebRenderVulkanVideo` capability defaults to false and is set
only when the selected backend, hardware-decoding policy and capability probe
permit native publication. These pool tests do not establish end-to-end
playback or performance.

## Capability probe

The startup probe queries the same mutable NV12 format, plane views, modifier,
filtering, readback and external-memory support as the importer. It reports
per-modifier dimension and allocation-size limits, plus Vulkan device/driver
identity, only when the selected Vulkan adapter matches the decoder DRM node.
Failed queries clear the result. GL and software WebRender skip the probe.
`WebRenderVulkanVideoCapabilities` carries these results through graphics IPC.

The native bridge tests compare this probe against the actual VA-API export
and verify device mismatch clears previously successful results. The importer
tests also check that the reported limits admit the frame they sample. Unit
tests cover modifier, dimension and allocation-size boundaries; the C++
`VideoCapabilitiesSurviveGfxVarIPC` test checks serialization of device identity
and both modifier records, including sizes above 4 GiB.

A successful probe alone does not enable publication. Live renderer registrations
must match its DRM node, device/driver UUIDs and format limits before native
video sampling. Each publication must fit the reported modifier and allocation
limits. Initial color admission permits BT.601/BT.709 matrices, ordinary SDR
primaries, BT.709 transfer and no HDR metadata. Image creation, memory
compatibility and actual plane layout are still validated at import.

Renderer errors, GPU-process loss and compositor device resets revoke
native-video capability for the browser session. The decoder checks revocation before feeding another
packet once it has native publications. Vulkan rendering selects only VA-API
through this path and preserves hardware-decoder preferences. C++ tests cover
frame rejection, incompatible simultaneous renderer registrations, removal of
those registrations and persistent capability revocation. A native Rust test
checks the actual device-registration payload and its removal.

`HardwareDecodeRecoveryTest.*` exercises the Linux media reader's first switch
to software when there is no later keyframe. Seekable sources replay from an
earlier keyframe and discard already delivered frames. The tests check frame
timestamps, software-only decoder selection, remote decoder crashes, failed
seeks, software decode failure and unseekable input.

Revocation prevents new native publications. An already-published, healthy
paused frame can still be sampled when its metadata fits every live renderer's
actual capabilities. This does not restore the publication capability. After
GPU-process replacement, images with obsolete image-bridge texture clients
use decoder readback for consumers that cannot resolve the old texture handle.

## GL readers

Native VA-API GL blits acquire the publication lock before creating plane
textures or drawing. They wait for a GL fence before releasing access. A busy
publication is waited on for up to five seconds without poisoning it on timeout;
an uncertain GPU completion
abandons it. CPU snapshots delegate source access to the same blit operation,
then read the owned destination buffer. Ordinary Vulkan playback does not use
this snapshot path.

`DMABufSurface.DISABLED_NativeVAAPIGLReaders` requires a live frame from
`ExportVAAPIFrame` and is disabled in ordinary test runs. It compares native
and legacy GL snapshot pixels for the same frame under BT.601/BT.709 and
limited/full-range interpretation. It also checks those pixels against the
exported NV12 bytes with an independent conversion, along with
busy/abandoned snapshots and the surface-descriptor blit used by WebGL uploads.
Run the exporter with a wrapper executable that ignores the Rust test arguments
and launches the compiled Firefox gtest binary with
`GTEST_FILTER=DMABufSurface.DISABLED_NativeVAAPIGLReaders`,
`GTEST_ALSO_RUN_DISABLED_TESTS=1` and `MOZ_RUN_GTEST=True`.
Use the existing Firefox gtest runtime environment and an available EGL driver.

This fixture tests GL-reader coordination independently of browser playback.
The browser acceptance runner below exercises the web-facing readers.

NV12 and planar 8-bit GL blits use a range-aware matrix. Texture-host CPU
readback also preserves the descriptor's range. Other blit inputs keep their
existing limited-range default until their callers supply a supported range.
`Colorspaces.GLBlitYUVMatrixHonorsRange` checks neutral endpoints and colored
values against independent BT.601/BT.709/BT.2020 equations;
`Colorspaces.GLBlitIdentityDoesNotExpandRange` covers GBR identity. The native
fixture uses nearest-neighbor chroma sampling to match the GL blit's sampler,
with a two-value tolerance for 8-bit channel rounding.

Native Vulkan video frames drain their sampling completion before returning
from frame end, so ownership can return even if no later frame or readback is
requested. An unexpected surviving publication or completion timeout fails
the renderer. This initial policy waits synchronously for native video;
frames without live native video publications keep the existing polling path.

Both GL readers and the HAL acquire callback use the bounded shared-lock wait.
The native GL fixture includes a reader waiting on another thread's access.
`DMABufSurface.VAAPIWait*` covers timeout without ownership changes, successful
wakeup across separate mappings, and abandonment. The native bridge test
`vaapi_nv12_frame_completion_releases_before_readback` checks that a rendered
publication is released without requiring a readback or another frame.

## Browser acceptance

The manual Marionette runner uses an already-built Firefox, an isolated
profile, and by default a private Xvfb display with Openbox. Install `xvfb-run`,
`xauth`, `openbox` and `xprop` before running it. It checks the actual compositor
backend and process, decoder hardware
status, native import diagnostics, and Vulkan validation output. It compares
compositor readback with independent canvas and WebGL video reads during
playback, looping and seeking. A software-decoder control is separate from the
native-decoder reference, since their decoded pixel values can differ.

Generate deterministic VP9 fixtures with a system FFmpeg containing libvpx:

```sh
python3 gfx/webrender_bindings/tests/make_native_video_fixtures.py \
  artifacts/native-video/browser-fixtures
```

The generator writes limited-range and full-range 8-bit clips, a 10-bit clip
for unsupported-format fallback, a clip that changes resolution, and
decoded-frame metadata. Run with a supported VA-API decoder and Vulkan adapter:

```sh
python3 gfx/webrender_bindings/tests/run_browser_native_video.py \
  --clip artifacts/native-video/browser-fixtures/pattern.webm \
  --output artifacts/native-video/browser-gpu \
  --gpu-process true
```

Use `--binary` for another object directory. Optional `--icd`,
`--validation-layers` and `--adapter` select the Vulkan test environment.
`--software-presentation` enables Mesa's CPU presentation path for Xvfb,
which has no DRI3. This changes presentation, not the selected Vulkan adapter;
the test still checks the actual backend and decoder. `--render-node` selects
the decoder DRM node. Hardware-decoder platform checks may still reject Xvfb;
that is a failed native run, not native coverage. Use software-decoder controls
and standalone hardware import tests in that case.
Use `--native-display` only when desktop tests are intended. It uses the
current display and existing window manager, without starting Xvfb or Openbox.
Repeat with `--gpu-process false` for parent-process composition. Reports,
screenshots and complete Firefox logs are written under `--output`.

The `--scenario` options are `basic`, `windows`, `pip`, `lifecycle`, `reset`
and `crash`. Lifecycle checks scaling and minimize/restore. Use
`--resolution-change` with `pattern-resize.webm` to assert that playback
crosses a coded-resolution change.
Reset checks retained paused pixels and subsequent software-decoder recovery.
Crash terminates the test browser's GPU process, flushes the replacement
compositor, recreates the page's lost WebGL context, and checks recovery.
The readback flush means this check does not establish unassisted repaint
timing. PiP checks the actual separate window's
pixels using `xwininfo` and ImageMagick `import`, targeted by a unique test
window title. These programs must be installed for that scenario.

Controls include `--backend gl`, `--backend software --decoder software`,
`--software-video --decoder software`, and
`--zero-copy-disabled --decoder software`. Use `--decoder software` with the
10-bit fixture to require fallback. These options express an expected decoder;
a failed native test is never silently accepted as a software success.

The initial native coverage is Intel VA-API and Vulkan with the admitted NV12
layouts. Other vendors, unsupported formats and playback performance require
separate evidence. Ordinary native playback samples the decoder allocation;
canvas readback, screenshots, WebGL uploads and recovery readback explicitly
copy pixels when their consumer needs a separate image.
