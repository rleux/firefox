# WebGPU DMA-BUF measurements

## Common workload

Measurements use a single Firefox/libxul build for copy and direct sampling,
separate Vulkan logical devices, and the same producer export and recycling
protocol. The producer-side copy remains. The benchmark presents a
1920x1080 premultiplied canvas containing opaque pixels. Its displayed content
capture is 890x705 device pixels. Native runs explicitly enforce device-pixel
ratio 1 and disable OS text-scale zoom in the private profile. Frame intervals exclude 15 warmup
frames; process counters include warmup. Reported cadence comes from animation
frame callbacks, not physical scanout timestamps.

Four copy/direct pairs run in alternating order for timing (6,000 frames per
run), followed by four separate diagnostic pairs (600 frames). Two memory
pairs use 1,200 frames. Timing uses light CPU/RSS/DRM sampling with per-frame
logging suppressed; memory uses full `smaps_rollup` sampling. Validation layers
are disabled. Every run checks renderer identity, selected transport and final
composited pixels. Host CPU, process activity, power policy, clocks and
available temperature sensors are recorded. No builds or unrelated tests run
concurrently.

See [the runner and import contract](WebGPUDMABuf.md) for commands and counter
semantics. The local experiment driver and raw reports are under
`artifacts/webgpu-zero-copy/controlled-ab/`.

## Xvfb baseline — 2026-09-18

Rendering and DMA-BUF import used the **Intel Iris Xe RPL-P hardware GPU** with
Mesa 26.2.2. Presentation used private Xvfb/Openbox and
`MESA_VK_WSI_DEBUG=sw`. The X server screen was 1280x1024; the browser outer
rectangle was 900x800. This baseline does not measure native desktop
presentation. All 20 accepted runs passed.

Measurement support is in commit `b24e3f1e62c`. The same binary was used in all
three phases:

- Firefox SHA-256: `7a426f20552200c878264902e3485bf8e9b63df98249e8a0629ede06eeb8bc30`.
- libxul SHA-256: `e42bef8fda3d4b5989978cd2c1cbd0052816571c73081bab426f20a46d1e2c39`.
- Timing/diagnostics: `controlled-ab/full-gpu-final/`.
- Memory: `controlled-ab/full-memory-final/`.

### Timing

| Pair | Path | Measured elapsed s | p95 ms | p99 ms | Firefox CPU s | Tracked render cycles |
| --- | --- | --- | --- | --- | --- | --- |
| 0 | copy | 101.916 | 17.28 | 17.38 | 66.47 | 288,134,001 |
| 0 | direct | 101.678 | 17.26 | 17.36 | 65.91 | 244,676,826 |
| 1 | direct | 101.814 | 17.22 | 17.30 | 82.05 | 269,791,066 |
| 1 | copy | 101.729 | 17.26 | 17.38 | 69.96 | 305,735,462 |
| 2 | copy | 102.220 | 17.22 | 17.32 | 89.73 | 325,116,651 |
| 2 | direct | 101.729 | 17.26 | 17.36 | 69.16 | 266,600,519 |
| 3 | direct | 101.966 | 17.22 | 17.30 | 87.84 | 270,631,024 |
| 3 | copy | 101.728 | 17.22 | 17.30 | 88.62 | 323,435,717 |

All median frame intervals were about 17.1 ms. Paired direct-minus-copy elapsed deltas were -0.238, +0.085, -0.492 and +0.238 seconds; their median was -0.076 seconds over roughly 102 seconds. The paired median p95 delta was -0.01 ms. These are effectively equal at this workload's refresh cap. The earlier 51.36 ms direct p95 did not recur.

Process CPU deltas changed sign: -0.56, +12.09, -20.57 and -0.78 seconds. They do not support a consistent CPU advantage for either path. Direct used fewer tracked DRM render-engine cycles in every pair, by 11.8–18.0% (median about 15.7%). These counters include all tracked browser clients, not isolated import wall time. Other recorded engine-cycle deltas were zero.

### Waits and buffer recycling

The periodic native histograms are cumulative; the last snapshot per process/thread/event was used, not the sum of printed snapshots. Observed coverage is 512 operations per run, 2048 per event/path across four runs. GPU-process termination can omit the final partial batch. Spans nest and must not be added as independent costs.

| Event | Mean ms | Maximum ms |
| --- | --- | --- |
| Copy submission/setup | 0.180 | 1.140 |
| Copy release wait | 2.696 | 11.574 |
| Copy DMA-BUF handler total | 2.921 | 11.812 |
| Direct sampling support query | 0.0158 | 0.116 |
| Direct acquire | 2.476 | 11.555 |
| Direct import, including acquire | 2.575 | 11.641 |
| Direct DMA-BUF handler total | 2.597 | 11.661 |
| Direct frame completion | 1.753 | 11.071 |
| Direct ownership return (nested) | 1.648 | 10.961 |
| Direct device poll | 0.00105 | 0.00644 |

Each diagnostic run recorded 4 allocations, 611 reuses, 0 retirement rejections and 3 unsuccessful recycling lookups, for 615 publications. All 615 unique generations were consumed except one direct run with 614. This demonstrates working recycling with four observed buffers; it does not establish a universally fixed four-buffer limit. The synchronous acquire and frame-completion/ownership-return waits remain. Recycling alone does not make that pipeline asynchronous.

### Memory

Every run had 75 full process-memory samples with complete process coverage.

| Pair | Peak PSS copy/direct MiB | Direct minus copy MiB | Peak private copy/direct MiB |
| --- | --- | --- | --- |
| Copy then direct | 598.65 / 599.16 | +0.51 | 441.60 / 442.13 |
| Direct then copy | 603.20 / 602.40 | -0.79 | 446.65 / 445.38 |

The previous approximately 54 MiB PSS difference did not recur. These approximately 20-second runs do not establish a long-term memory plateau. Their p95 remained 17.26–17.28 ms, so full sampling did not reproduce the earlier pacing problem here. This does not rule out instrumentation sensitivity in longer or different workloads. The memory-sampling/map-lock mechanism remains a hypothesis, not an identified cause.

### Host conditions and limits

AC remained online, CPU governor was `performance`, platform profile was `balanced`. Whole-host CPU busy fraction was 6.56–8.46%; runnable median was 2. Xe frequency medians were 300–350 MHz, with maxima 500–600 MHz. Core-temperature medians were 60–64 C, with maxima 73–84 C. Desktop Xorg/window manager/terminal and Codex remained present and monitored. Xvfb/Openbox and test harness activity are part of test overhead. There was no clearly unique unrelated CPU spike explaining the path differences; CPU variation was chiefly in the test GPU process/Xvfb. Original pairs are retained without selecting results based on performance.

No whole-GPU busy sensor was available, so GPU contention cannot be ruled out. The results correct the earlier causal inference, not prove a general speedup or identify the earlier confound. Separate Vulkan logical devices and the producer-side export copy remain.

## Native X11 hardware presentation — 2026-09-18

The same Firefox and libxul hashes as the Xvfb baseline were used. The native
run used `DISPLAY=:0.0`, the existing Xfwm window manager and physical eDP-1
at 1920x1200/60 Hz. `xdpyinfo` confirmed DRI3; `xrandr --current` captured the
active mode. Every report identified Intel Iris Xe RPL-P/Mesa 26.2.2 and the
Vulkan renderer. `MESA_VK_WSI_DEBUG` was unset and no software-presentation
option was used. The private profile disabled OS text-scale zoom to enforce
DPR 1. The content viewport and captured pixels were 890x705, with a 942x843
outer window. Every accepted run stayed visible and focused, with no recorded
focus or visibility changes.

All 20 accepted runs passed with the common protocol: eight timing, eight
diagnostic and four memory runs. Raw records are in
`controlled-ab/native-full-final/` and `controlled-ab/native-memory-final/`.
Failed setup probes for the Marionette window-rectangle API and OS zoom were
excluded; they never entered the benchmark. The corrected smoke passed all
four copy/direct timing/diagnostic cases.

### Native timing

| Pair | Path | Measured elapsed s | p95 ms | p99 ms | Firefox CPU s | Tracked render cycles |
| --- | --- | --- | --- | --- | --- | --- |
| 0 | copy | 99.98202 | 17.24 | 17.32 | 18.42 | 338,395,323 |
| 0 | direct | 99.98278 | 17.22 | 17.30 | 21.16 | 300,248,162 |
| 1 | direct | 99.98184 | 17.24 | 17.30 | 21.24 | 291,542,575 |
| 1 | copy | 99.98178 | 17.24 | 17.34 | 18.60 | 336,030,882 |
| 2 | copy | 99.98240 | 17.24 | 17.30 | 18.46 | 341,276,442 |
| 2 | direct | 99.98138 | 17.24 | 17.30 | 20.13 | 311,471,555 |
| 3 | direct | 99.98186 | 17.24 | 17.30 | 20.45 | 320,332,517 |
| 3 | copy | 99.98236 | 17.24 | 17.34 | 18.43 | 334,529,527 |

The median paired direct-minus-copy elapsed difference was -0.00022 seconds;
p95 was effectively identical. No run had an interval above 34 ms. This is
matched animation-frame cadence at the 60 Hz cap, not a measurement of
physical scanout latency or maximum throughput.

Direct used more Firefox process CPU in every pair: +2.74, +2.64, +1.67 and
+2.02 seconds per 6,000 frames, or 9.0–14.9%. The paired median was +2.33
seconds (about 12.6%, or 0.388 ms CPU per frame). The excess was in the GPU
process. Direct used fewer tracked render-engine cycles in every pair by
4.2–13.2%; the median reduction was 33.98 million cycles, about 10.1% of the
copy median. Other tracked engine-cycle deltas were zero. CPU time, GPU cycles
and sleeping wall time are different quantities and must not be conflated.

### Native waits and recycling

The last periodic histograms cover 512 operations per diagnostic run, 2048
per event/path in aggregate. Nested spans are not independent costs.

| Event | Mean ms | Maximum ms |
| --- | --- | --- |
| Copy submission/setup | 0.1423 | 1.257 |
| Copy release wait | 1.929 | 8.781 |
| Copy DMA-BUF handler total | 2.109 | 9.030 |
| Direct sampling support query | 0.0152 | 0.394 |
| Direct acquire | 1.667 | 9.244 |
| Direct import, including acquire | 1.759 | 9.329 |
| Direct DMA-BUF handler total | 1.779 | 9.354 |
| Direct frame completion | 7.073 | 15.434 |
| Direct ownership return (nested) | 2.670 | 11.584 |
| Direct device poll | 0.00287 | 0.0330 |

Direct's separate frame-completion point was substantially longer than under
Xvfb (7.073 versus 1.753 ms), consistent with native presentation
synchronization. Acquisition is included in direct import/handler time;
ownership return occurs within frame completion. These measurements do not
establish the sleeping waits as the cause of the measured CPU excess.

Every direct diagnostic run had four allocations, 611 reuses and three
unsuccessful recycling lookups. Copy had five allocations, 610 reuses and
four unsuccessful lookups. Neither path rejected retirement. Each published
615 generations; all were consumed except copy pairs 0 and 2, which consumed
614. Thus recycling works, with one extra observed buffer in the native copy
path; synchronous completion still prevents a fully asynchronous pipeline.

### Native memory

Each run had 74 full-memory samples with complete process coverage.

| Pair | Peak PSS copy/direct MiB | Direct minus copy MiB | Peak private copy/direct MiB |
| --- | --- | --- | --- |
| Copy then direct | 609.354 / 609.356 | +0.003 | 433.316 / 434.078 |
| Direct then copy | 614.438 / 610.756 | -3.682 | 438.984 / 434.516 |

Both paths took about 19.983 seconds, with p95 intervals of 17.20–17.26 ms.
The earlier 54 MiB direct-path PSS penalty did not recur. These short runs do
not establish a long-term memory plateau.

### Native host conditions

AC remained online, CPU governor was `performance`, and platform profile was
`balanced`. Whole-host busy fraction was 3.89–3.97% for copy and 3.93–3.96%
for direct; paired differences were -0.041 to +0.064 percentage points.
Runnable maxima were 6–9. Xe frequency medians were 300–350 MHz with maxima
450–600 MHz; core-temperature median was 60 C with maxima 70–88 C. Xorg,
the window manager and terminal remained monitored. Xorg work is expected
presentation overhead for these native runs. No whole-GPU busy sensor was
available, so unrelated GPU contention cannot be ruled out.

## Presentation-mode comparison

These are medians of the four timing runs per path. Browser content and source
texture dimensions match, but the outer window, desktop compositor and
presentation mechanism differ. Cross-display totals are not a pure measure
of WSI cost; the copy/direct comparisons within each mode are better controlled.

| Metric | Xvfb copy | Xvfb direct | Native copy | Native direct |
| --- | --- | --- | --- | --- |
| 6,000-frame elapsed s | 101.822 | 101.771 | 99.982 | 99.982 |
| Run p95 frame interval ms | 17.24 | 17.24 | 17.24 | 17.24 |
| Firefox process CPU s | 79.290 | 75.605 | 18.445 | 20.805 |
| Tracked render cycles, millions | 314.6 | 268.2 | 337.2 | 305.9 |
| Host busy fraction | 7.57% | 7.24% | 3.92% | 3.94% |

Xvfb CPU differences changed sign between pairs. Native direct import had a
consistent CPU excess while reducing tracked render cycles. Neither mode
reproduced the earlier large direct-path frame-time or PSS penalty. Removing
the per-frame CPU waits remains an optimization opportunity; these results
do not justify dropping the ownership and recycling guarantees.
