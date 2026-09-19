# Native video asynchronous-transfer measurements

Measured on 2026-09-19 after the implementation in `64f5f122792` passed the
[native video correctness checks](NativeVideo.md). All 20 measurement runs
passed their decoding, transport, presentation and final-pixel checks, with no
reported dropped frames or focus/visibility changes.

Asynchronous transfer did not demonstrate a CPU or playback-throughput benefit
in this workload. Firefox process CPU increased in three of four timing pairs;
the median paired increase was 1.975 seconds per 100 seconds of playback,
approximately 3.1% of synchronous CPU usage. Both modes reported zero dropped
frames. This is a local result with substantial background desktop activity.

## Controlled setup

Both modes use the same Firefox binary, direct NV12 sampling, driver, clip and
profile settings. `WR_VIDEO_FORCE_SYNC=1` selects synchronous acquire, ownership
return and frame-end completion; async leaves it unset. Both retain the decoder's
`vaSyncSurface` readiness wait and add no decoded-frame handoff copy.

- Native Xorg/DRI3, physical eDP-1 at 1920x1200 and 60 Hz.
- Vulkan WebRender (`wgpu-hal`), Intel Iris Xe Graphics (RPL-P), Mesa 26.2.2.
- GPU-process rendering; content viewport 890x705, device-pixel ratio 1.
- System Vulkan loader `/usr/lib/x86_64-linux-gnu/libvulkan.so.1.3.275`.
- No validation layers or software WSI during performance measurements.
- Animated 1920x1080, 60 fps VP9 Profile 0 clip, 120 seconds, no audio,
  8-bit YUV420, limited-range BT.709. The video scales to the viewport width.
- Two seconds of playback warmup before each fixed wall-time interval.
- No canvas/WebGL readers or screenshots during timing. A paused final frame
  is compared through canvas readback and a compositor screenshot afterward.
- Actual hardware decoding, direct NV12 transport, sync mode, loader mapping,
  viewport, visibility and focus are checked per run.

Firefox launcher SHA-256:
`7f6c7582f473f8e45d70ab5dfc40d80df79841a1b83d65acf4a42a8dbdfab83b`

`libxul.so` SHA-256:
`a7020a4c7aad3be41dc258fe16147414054767c60ddf58e98e6b4fe1ba1e6a39`

Clip SHA-256:
`02e9e0bfde44c3da5ca6ef74c681277ccba734655661d48accec77a0f56f6b6c`

Manifests also record the harness hashes and clip metadata. No builds or other
GPU tests ran concurrently. Host monitoring includes five seconds before and
after each browser run.

## Primary timing: four balanced pairs

Each run measures 100 seconds of playback after warmup. Light process sampling
records CPU, summed RSS and DRM counters; per-frame logs and synchronization
instrumentation are disabled. Process sampling brackets the measured operation,
excluding browser startup, warmup and final readback.

| Pair/order | Sync CPU (s) | Async CPU (s) | Async minus sync (s) | Async CPU change |
| --- | ---: | ---: | ---: | ---: |
| 0: sync, async | 63.05 | 65.22 | +2.17 | +3.44% |
| 1: async, sync | 63.99 | 65.77 | +1.78 | +2.78% |
| 2: sync, async | 63.78 | 62.99 | -0.79 | -1.24% |
| 3: async, sync | 62.66 | 67.43 | +4.77 | +7.61% |

The synchronous CPU median is 63.415 seconds. The median paired increase of
1.975 seconds is about 0.329 ms per video frame at 60 fps. It measures CPU
consumption, not time blocked or GPU execution latency. The mixed pair signs
and background load limit attribution; these runs do not support a CPU-saving
claim for asynchronous video transfer.

Each run reports 6,000 or 6,001 total video frames and zero dropped frames.
JavaScript observes only 2,476–2,478 `requestVideoFrameCallback` callbacks, whose
`presentedFrames` values advance by 5,996–5,998 between the first and last
observation. Callback intervals have p95 values of 40.52–40.60 ms in both modes.
Callbacks therefore skip observations of submitted frames: their cadence must
not be interpreted as video frame rate, frame latency or scanout timing. The
workload remains refresh-capped, with no demonstrated throughput improvement.

| Pair | Sync render cycles (million) | Async render cycles (million) | Sync video cycles (million) | Async video cycles (million) |
| --- | ---: | ---: | ---: | ---: |
| 0 | 213.405 | 169.864 | 165.468 | 142.017 |
| 1 | 202.713 | 223.471 | 157.473 | 173.157 |
| 2 | 190.044 | 175.265 | 155.578 | 136.478 |
| 3 | 179.954 | 226.323 | 148.630 | 174.673 |

These are deduplicated per-client `drm-cycles-rcs` and `drm-cycles-vcs` deltas.
Both counters increase in two pairs and decrease in two. They do not establish
a consistent GPU-work reduction or measure whole-GPU utilization.

## Separate synchronization diagnostics

Four balanced pairs measure ten seconds of playback with synchronization
instrumentation and process sampling disabled. The table combines the last
cumulative histogram per process, thread and event. Logging starts before
warmup; these counts are not restricted to the timed interval, and the last
partial batch is not emitted.

| CPU-side wall span | Samples per mode | Sync mean (ms) | Async mean (ms) |
| --- | ---: | ---: | ---: |
| Import handler, including acquisition where needed | 5,632 | 1.5164 | 0.3069 |
| Acquisition | 2,560 | 2.5097 | 0.1209 |
| Ownership return | 2,560 | 2.0790 | 0.1247 |
| Frame completion | 2,560 sync; none async | 5.2699 | — |

Async removes the unconditional frame-end drain and measures queue submission
instead of waiting for the acquire/return fence. These shorter wall spans do
not imply reduced CPU consumption. The spans overlap and have different
counts: import runs for each plane lease, while acquisition runs only when
needed. Do not add the rows together. No `frameBackpressure` or
`videoPublicationReuseWait` histogram appeared; that does not prove these
events never occurred below the reporting threshold.

## Separate memory sampling

Two balanced pairs measure 20 seconds of playback with full process-memory
sampling and quiet logs. Every observed Firefox process is covered at each
sample. Their CPU measurements are excluded from the primary comparison.

| Pair/order | Sync peak PSS (MiB) | Async peak PSS (MiB) | Async minus sync | Sync peak private (MiB) | Async peak private (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: |
| 0: sync, async | 630.18 | 632.62 | +2.44 | 447.62 | 449.12 |
| 1: async, sync | 631.88 | 631.34 | -0.54 | 449.29 | 449.13 |

There is no consistent memory improvement. PSS falls from the first to last
sample in all four runs. These short intervals do not establish a long-term
memory plateau or total GPU-memory usage; summed RSS can double-count shared
pages.

## Host conditions and limits

The host uses a Core i5-1340P with AC power online, the `powersave` CPU governor
and `balanced` platform profile. Unlike the earlier WebGL measurements, these
runs did not use the `performance` governor; absolute costs across the two
workloads are not comparable. Recorded GPU clock medians range from 317 to
450 MHz. Clocks were observed, not fixed.

Whole-host CPU busy fraction ranges from 9.79% to 10.41% during timing runs,
including their setup and settling periods. Persistent desktop processes
consume substantial CPU: for the first pair, Xorg uses approximately 31–32 CPU
seconds, the terminal 20–21 seconds, the window manager about seven seconds,
and xfconfd about five seconds per run. This was not an idle host. The monitor
records CPU contention, but no whole-GPU busy sensor is available, so other GPU
contention cannot be ruled out.

The process sampler brackets each 100-second measured interval with a
100.045–100.072-second window. It sums CPU counters of live Firefox descendants;
exited children can therefore lose accumulated CPU from the total. Counters
remain monotonic in these runs, but a transient 13th process appears and exits
around the middle of each run. Monotonicity alone does not exclude this
undercount. Treat the small CPU differences as measurements under these host
and sampling conditions, not an isolated causal estimate or a general speedup.

The diagnostic results establish reduced consumer blocking. They do not explain
the process-CPU differences, measure the remaining decoder wait, or establish
lower presentation latency. Other devices, drivers and playback workloads need
separate evidence.

## Reproduction and artifacts

Generate the clip with the command in [NativeVideo.md](NativeVideo.md#transfer-benchmark).
For an individual timing run:

```sh
python3 gfx/webrender_bindings/tests/run_video_transfer_benchmark.py \
  --clip artifacts/video-async/benchmark-1080p60.webm \
  --binary obj-x86_64-pc-linux-gnu/dist/bin/firefox \
  --output artifacts/video-async-timing --display native --viewport 890 705 \
  --synchronization async --duration 100 --process-metrics light --quiet \
  --icd /usr/share/vulkan/icd.d/intel_icd.json
```

Repeat with `sync`, alternating pair order. Diagnostic runs use `--duration 10
--process-metrics off --sync-instrumentation` without `--quiet`; memory runs use
`--duration 20 --process-metrics full --quiet`. The local controller
`artifacts/webgpu-zero-copy/controlled-ab/run.py --api video` orchestrates the
20-run protocol and host monitoring. Its `--frames`, `--diagnostic-frames` and
`--memory-frames` values are divided by 60 to obtain fixed wall-time durations
for video; they do not set a callback-count termination condition.

Raw manifests, commands, reports, logs, host samples and analyses are under:

- `artifacts/video-async/native-full/` — eight timing and eight diagnostic runs.
- `artifacts/video-async/native-memory/` — four memory runs.

Validation-loader smokes are separate from these system-loader performance
datasets. WebGPU W4 performance remains a separate pending comparison.
