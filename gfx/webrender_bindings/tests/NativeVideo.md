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
