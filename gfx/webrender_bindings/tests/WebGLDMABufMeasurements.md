# Native WebGL asynchronous-transfer measurements

Measured on 2026-09-19 after the implementation in `9f50c1d04e6` passed the
[WebGL correctness checks](WebGLDMABuf.md). All 20 measurement runs passed.

Asynchronous ownership transfers reduced Firefox process CPU in all four timing
pairs. The median paired saving was 1.13 seconds per 6,000 frames, approximately
5% or 0.188 ms per frame. Refresh-capped cadence was unchanged. These runs did
not demonstrate a consistent GPU-cycle or memory improvement.

## Controlled setup

Both modes used the same Firefox binary, direct DMA-BUF sampling, driver,
workload, profile settings and display. `WR_WEBGL_FORCE_SYNC=1` selected the
synchronous acquire/return control; asynchronous mode left it unset. Neither
mode introduced a WebRender-side import copy.

- Native Xorg/DRI3, physical eDP-1 at 1920x1200 and 60 Hz.
- WebGL producer: Mesa Intel Iris Xe Graphics (RPL-P).
- WebRender consumer: Vulkan (`wgpu-hal`), Intel Iris Xe, Mesa 26.2.2.
- GPU-process rendering; content viewport 890x705, device-pixel ratio 1.
- System Vulkan loader `/usr/lib/x86_64-linux-gnu/libvulkan.so.1.3.275`.
- No Vulkan validation or software WSI during measurements. The fixed loader
  used for validation tests was not used for performance.
- WebGL 2, 1920x1080 canvas, two clears per frame, opaque colors encoding the
  frame index. Fifteen warmup frames precede the measured samples.
- No per-frame screenshots, readbacks, `finish`, or CPU waits on GPU queries.
  The final composited screenshot verifies the expected frame index.
- Every run verified direct transport, the selected sync mode, hardware
  renderer metadata, loader mapping, viewport, visibility and focus. No blur
  or visibility-change events occurred.

Firefox launcher SHA-256:
`41b71c8e8c0d81b9c7a4f1a6a6d7d429c749a349d0b1c38d753cfe13c957d537`

`libxul.so` SHA-256:
`12ad3e8ac23fe0072b042c28969a0341d235df3ec0e310b4048676209d7d0d27`

The manifests also contain hashes of the runner, test page and process sampler.
No builds or unrelated tests ran during measurement. Host sampling includes
five seconds before and after each browser run.

## Primary timing: four balanced pairs

Each run contains 6,000 measured animation-frame samples. Light process
sampling records CPU, summed RSS and DRM counters; per-frame logging and
synchronization diagnostics are disabled. Process counters include warmup and
setup within the measured operation. Intervals below are animation-frame
intervals, not hardware presentation-latency measurements.

| Pair/order | Sync CPU (s) | Async CPU (s) | Async CPU change | Sync p95 (ms) | Async p95 (ms) |
| --- | ---: | ---: | ---: | ---: | ---: |
| 0: sync, async | 22.45 | 21.31 | -5.08% | 17.22 | 17.24 |
| 1: async, sync | 22.33 | 21.21 | -5.02% | 17.24 | 17.20 |
| 2: sync, async | 22.33 | 20.69 | -7.34% | 17.20 | 17.24 |
| 3: async, sync | 22.37 | 22.05 | -1.43% | 17.18 | 17.22 |

The median paired CPU difference is -1.13 s, or 5.1% of the synchronous CPU
median of 22.35 s. All runs take approximately 99.982 s between the first and
last measured frame samples; the median paired elapsed difference is -0.00071 s.
No run has an interval over 34 ms. Median paired p95 and p99 differences are
+0.03 ms and +0.02 ms respectively. There is no demonstrated frame-throughput or tail-latency
benefit at this refresh cap.

| Pair | Sync render cycles (million) | Async render cycles (million) | Async change |
| --- | ---: | ---: | ---: |
| 0 | 561.280 | 570.253 | +1.60% |
| 1 | 572.554 | 565.622 | -1.21% |
| 2 | 565.214 | 564.753 | -0.08% |
| 3 | 563.058 | 559.178 | -0.69% |

These are deduplicated per-client `drm-cycles-rcs` deltas. The median paired
difference is approximately -2.17 million cycles (-0.38%), with mixed signs.
They do not establish a GPU-work reduction or whole-GPU utilization.

## Separate synchronization diagnostics

Four balanced pairs use 600 measured frames with synchronization instrumentation
and process sampling disabled. The last cumulative histogram per process,
thread and event supplies 512 observed operations per run: 2,048 per event and
mode. The final partial batch was not emitted, so these counts do not cover
every publication.

| CPU-side wall span | Sync mean (ms) | Async mean (ms) |
| --- | ---: | ---: |
| Import handler, including acquisition | 4.3250 | 0.1581 |
| Acquisition | 4.2166 | 0.0559 |
| Ownership return | 1.9363 | 0.0504 |

The asynchronous spans measure submission work rather than waiting for GPU
completion. The import span includes acquisition; do not add them together.
Wall-clock blocking is not CPU time, and these spans alone do not establish the
cause or size of the process-CPU saving above. No `frameBackpressure` or
`webglPublicationReuseWait` samples appeared in these runs. Forced pressure is
covered separately by deterministic tests.

## Separate memory sampling

Two balanced pairs use 1,200 measured frames and full process-memory sampling.
All observed Firefox processes were covered at each sample. CPU measurements
from these instrumented runs are not included in the primary timing comparison.

| Pair/order | Sync peak PSS (MiB) | Async peak PSS (MiB) | Async minus sync | Sync peak private (MiB) | Async peak private (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: |
| 0: sync, async | 607.72 | 609.56 | +1.84 | 429.82 | 431.02 |
| 1: async, sync | 610.46 | 608.40 | -2.06 | 432.51 | 430.78 |

PSS decreased from the first to last sample in every run. There is no consistent
PSS/private-memory difference. These approximately 20-second samples do not
establish a long-term memory plateau or total GPU-memory usage. Summed RSS can
double-count shared pages.

## Host conditions and limits

During primary timing, whole-host CPU busy fraction ranged from 4.10% to 4.28%.
Median values were 4.145% for sync and 4.160% for async; paired differences were
small and mixed. AC power was online, the CPU governor was `performance` and
the system power profile was `balanced`. Recorded Xe clock medians were
350-367 MHz; CPU temperature medians were 61-62 degrees C, with transient
maxima reaching 90 degrees C. Clocks and temperatures were recorded, not held
fixed. A whole-GPU busy sensor was unavailable, so other GPU contention cannot
be ruled out.

This is a local, refresh-capped result on one device/driver. It supports a CPU
reduction for this workload, not a general speedup, reduced presentation latency
or a gain on other drivers. WebGL producer presentation/resolve work remains.

## Reproduction and artifacts

For an individual timing run:

```sh
python3 gfx/webrender_bindings/tests/run_browser_webgl.py \
  --binary obj-x86_64-pc-linux-gnu/dist/bin/firefox \
  --output artifacts/webgl-sync-timing --display native --viewport 890 705 \
  --synchronization sync --benchmark-only --benchmark-frames 6000 \
  --process-metrics light --icd /usr/share/vulkan/icd.d/intel_icd.json
```

Repeat with `async`, alternating pair order. Use separate 600-frame diagnostic
and 1,200-frame memory runs as described above. The local controlled runner
`artifacts/webgpu-zero-copy/controlled-ab/run.py --api webgl` orchestrated the
20-run protocol and host monitoring.

Raw manifests, per-case commands/reports/logs, host samples and analyses are in:

- `artifacts/webgl-async/native-full-final/`
- `artifacts/webgl-async/native-memory-final/`

Earlier Xvfb and harness-development failures are excluded from these datasets.
The pending WebGPU W4 performance comparison is a separate measurement and was
not run as part of this WebGL session.
