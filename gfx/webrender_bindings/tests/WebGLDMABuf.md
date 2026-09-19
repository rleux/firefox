# Asynchronous WebGL DMA-BUF import

WebGL's GL/EGL producer exports a linear RGB DMA-BUF and a native fence.
WebRender imports and samples that allocation directly. Ownership transfers use
[`FOREIGN_EXT`](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_queue_family_foreign.html),
which supports foreign APIs; WebGPU's Vulkan exports use the separate EXTERNAL
contract. The existing WebGL presentation and resolve work is unchanged, so
direct WebRender import is not a claim of end-to-end copy-free presentation.

The consumer submits the producer fence wait and acquire barriers on the same
Vulkan queue as subsequent rendering. Import returns without a CPU completion
wait. After the final image lease and GPU read complete, it queues the ownership
return. The callback keeps the C++ texture host and publication lock alive until
that return completes. Uncertain completion abandons the publication and prevents
producer reuse.

The cache retains allocation, generation, layout, handler and logical-device
identity while a return is pending, even after the weak image expires. A retry
of that publication polls the pending return outside the cache borrow before
relocking it; mismatched live publications remain rejected. Retry waits have a
five-second timeout.

W4's draw/return completion tracking and idle polling also serve WebGL. Draw and
external-image submission queues and pending frame records each have a limit of
three. WebGL retains its existing remote-texture recycling and shared-surface
reference checks; the eight-export producer limit documented for WebGPU does
not apply to the WebGL swap chain. Shutdown drains or abandons pending work.
Driver-level queue/device teardown can still block inside the Vulkan driver.

## Controls and diagnostics

The renderer remains opt-in through `gfx.webrender.vulkan`. For a same-binary
comparison, `WR_WEBGL_FORCE_SYNC=1` restores the synchronous acquire and return
operations. Leaving it unset selects asynchronous transfers. Both modes import
and sample the DMA-BUF directly. This control does not change WebGPU or NV12
ownership transfers.

Each render thread emits a one-time marker only after a successful import:

```text
WebRender Vulkan WebGL selected transport: direct; synchronization: async
```

The synchronous control prints `sync` instead. `WR_WEBGL_BENCHMARK_QUIET=1`
suppresses per-frame logging while retaining this marker.
`WR_WEBGL_SYNC_INSTRUMENTATION=1` enables cumulative wall-time histograms with
the prefix `WebGL DMA-BUF sync metrics:`. Events include `webglImport`, `acquire`,
`ownershipRelease`, `webglPublicationReuseWait` and shared `frameBackpressure`.
Use the last sample per process/thread/event. Nested spans must not be added
as independent costs, and asynchronous submission duration is not GPU execution
or presentation latency.

## Validation and measurement

The real GL fixture checks RGBA/BGRA exports, repeated generations, flipped
sampling and subsequent GL reuse. Deferred-completion tests verify that the
publication stays retained until the ownership return completes. Run the native
and Naga shader variants with Vulkan validation; use a fixed Vulkan loader for
validation-enabled multi-device tests as described in [the WebGPU notes](WebGPUDMABuf.md).

The browser runner uses a private Xvfb display by default. This host's Xvfb lacks
DRI3, so it cannot exercise Firefox's hardware WebGL DMA-BUF producer. Correct
fallback pixels do not count as native coverage: the runner requires the direct
import marker and fails without it. Native browser acceptance therefore requires
a hardware-capable display, after user readiness confirmation. The standalone
GBM/EGL fixture uses the real GPU without a visible display.

Browser correctness cases exercise composited pixels and explicit snapshots.
Benchmark mode avoids per-frame screenshots, `readPixels`, `finish` and CPU
waits on GPU queries. Clear colors encode the frame index, and the final
composited screenshot checks the expected index so frozen initial output cannot
pass. Rendering uses a 1920x1080 canvas with 15 warmup frames.

Compare sync and async using the same binary, loader, driver, viewport, workload
and warmup, with both pair orders. Separate primary timing, synchronization
diagnostics and full memory sampling. Native measurements require an idle host
and a visible, focused test window; record host load and actual loader/adapter
mappings. Refresh-capped frame cadence alone does not establish uncapped
throughput or prove that removing CPU waits reduced total CPU use.

For native correctness, after the host is ready:

```sh
python3 gfx/webrender_bindings/tests/run_browser_webgl.py \
  --output artifacts/webgl-async-native --display native \
  --viewport 890 705 --synchronization async --scenario lifecycle
```

Repeat with `--synchronization sync` and `--gpu-process false`. Additional
scenarios are `windows`, `context-loss` and `offscreen`; the latter covers both
main-thread and worker OffscreenCanvas, including worker termination.
Use `--validation-layers` and `--loader-directory` for correctness runs.
For timing use `--benchmark-only --benchmark-frames 6000 --process-metrics light`
without validation. Use separate `--sync-instrumentation --process-metrics off`
and `--process-metrics full` runs for diagnostics and memory.

Current validation: Firefox export/binary builds passed, as did 14 real GL
fixture runs across native/Naga shaders and sync/async controls, including
delayed return, first-read pixels, queue pressure, shutdown, abandonment and
cache reuse. WebGPU direct-import browser controls passed all 24 cases in both
GPU and parent processes. Native WebGL browser acceptance passed sync/async basic
cases in GPU and parent processes, plus asynchronous resize, multiple windows,
same-context restoration and main-thread/worker OffscreenCanvas termination and
recovery. Each correctness run checks 16 base pixel cases and preserved-buffer
snapshots. Both 120-frame benchmark smoke runs passed the frame-index,
direct-transport, mode, focus, viewport and system-loader checks. Long-run
measurements are recorded in [the native measurement report](WebGLDMABufMeasurements.md).
The 20-run comparison observed about 5% lower process CPU with asynchronous
transfers, unchanged refresh-capped cadence, and no consistent GPU-cycle or
memory difference. Failed Xvfb probes are excluded from native
acceptance. Logs are under `artifacts/webgl-async/`.
