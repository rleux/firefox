# Native VA-API video tests

The experimental importer maps one completed, same-device NV12 DMA-BUF as a
mutable Vulkan image. R8 and RG8 views share that image, memory, layout state
and publication lifetime. WebRender samples those views with its YUV shaders;
the importer does not copy the frame into owned plane textures. Explicit
snapshots and test reference readbacks still copy pixels.

The initial subset is even-sized NV12 with linear or Intel Y-tiled memory,
one object and two memory planes. The driver must support the exact modifier,
sampled usage, filtering, view formats and foreign queue ownership. Browser
publication stays disabled until the bridge, pool reuse and fallback steps
are implemented and validated.

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
tests are described below; browser playback still requires capability negotiation
and fallback integration.

Native tests compare decoded Y/UV bytes and rendered pixels with controls,
including nearest/linear modes, cropped visible dimensions and downscaling.
They verify shared image identity, retention while either plane lease is alive,
device mismatch, second-view cleanup and injected submission failure.
Pure layout/release tests use the binary's `nv12_` filter without the fixture.

The kernel can round the backing allocation above the decoder-reported size.
That padding is accepted, but plane bounds and Vulkan memory requirements must
fit the reported size, and the report must fit the actual backing object.

Separate-object NV12, P010, cross-device import and protected frames remain
unsupported by this importer. These tests establish HAL behavior, not browser
decoder publication, pool recycling or playback performance.

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

The internal `UseWebRenderVulkanVideo` capability defaults to false. Production
publication remains disabled until negotiation and fallback are implemented.
Separate consumer paths, including WebGL video uploads and live snapshots, still
need browser validation before enablement. These pool tests do not establish
end-to-end playback or performance.

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

A successful probe does not enable publication. Live renderer registrations
must match its DRM node, device/driver UUIDs and format limits before native
video sampling. Each publication must fit the reported modifier and allocation
limits. Initial color admission permits BT.601/BT.709 matrices, ordinary SDR
primaries, BT.709 transfer and no HDR metadata. Image creation, memory
compatibility and actual plane layout are still validated at import.

Renderer errors and compositor device resets revoke native-video capability
for the browser session. The decoder checks revocation before feeding another
packet once it has native publications. Vulkan rendering selects only VA-API
through this path and preserves hardware-decoder preferences. C++ tests cover
frame rejection, incompatible simultaneous renderer registrations, removal of
those registrations and persistent capability revocation. A native Rust test
checks the actual device-registration payload and its removal.

Publication remains disabled pending recovery and consumer validation. In
particular, Linux's existing hardware-decoder fallback can fail after playback
starts when there is no later keyframe; revoking capability alone does not
establish successful software-decoder recovery for that case.
