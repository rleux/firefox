# Local renderer measurement harness

`run_renderer_benchmark.py` uses the existing Marionette interface and a fixed
local page to compare opt-in Vulkan WebRender with retained OpenGL. It creates
no WebGL, WebGPU or video producer. It is a local diagnostic harness; it does
not replace full pixel acceptance or a production performance suite.

The default is a short correctness smoke on private Xvfb/Openbox. Each invocation
creates a new output directory and browser profile, explicitly selects the
startup-bound backend preference and checks the actual backend/process/renderer,
viewport, DPR, visibility, focus and final composited pixels. Unexpected software
rendering fails unless `--allow-software` is supplied for a smoke/diagnostic run.
The GL and Vulkan cases must use the same binary, scene, viewport and process mode.

Example software correctness control, with paths chosen for the local machine:

```sh
python3 gfx/webrender_bindings/tests/run_renderer_benchmark.py \
  --binary artifacts/stage9/b7ba23c55b6/baseline/firefox/firefox \
  --output artifacts/stage9/b7ba23c55b6/smoke-vulkan-static \
  --backend vulkan --workload static --allow-software \
  --icd /usr/share/vulkan/icd.d/lvp_icd.json
```

The local validation loader and layers can be supplied through
`--loader-directory` and `--validation-layers` for correctness only. These flags
are rejected for timing/memory, as are Xvfb, software presentation and software
renderer admission. Native timing requires an existing X11 `DISPLAY`, an idle
host, no competing builds/tests, and explicit user readiness before starting.

```sh
python3 gfx/webrender_bindings/tests/run_renderer_benchmark.py \
  --binary artifacts/stage9/b7ba23c55b6/baseline/firefox/firefox \
  --output artifacts/stage9/b7ba23c55b6/timing-vulkan-css \
  --backend vulkan --display native --renderer Intel \
  --workload css --phase timing --warmup 10 --duration 60 \
  --icd /usr/share/vulkan/icd.d/intel_icd.json
```

These are single-run commands, not a completed experiment. A comparison needs
balanced paired repetitions, frozen run-validity rules, correctness prerequisites
and separate memory/diagnostic series. Never compare hardware Vulkan against
software GL as an API-overhead claim. Preserve all data and report failed or
unsupported cases. No measurements are implied by the existence of this harness.

## Workload and interval semantics

`static` schedules only the interval-ending timer and performs no rAF updates.
`css` animates a transformed tile; `dirty` alternates a tile's color; `scroll`
updates a fixed scroll container; `canvas` repaints a 512×512 2D canvas; `filters`
animates clipped, filtered text/gradient content. The active cases collect rAF
intervals. These callbacks measure page scheduling and workload updates, not
completed GPU frames or display scanout. The canvas case must not be called an
isolated texture-upload benchmark without diagnostic evidence of that path.

The canvas workload explicitly disables accelerated Canvas2D and its force-enable
preference on both backends, and verifies those effective settings. This keeps
the producer policy fixed while WR remains selected independently. It does not
measure accelerated Canvas2D or a native WebGL/WebGPU transfer path.

Timing and memory runs first wait until the Firefox parent is at least 65 seconds
old, then require an unchanged PID/start-time process tree for five seconds.
This lets scheduled startup maintenance, including crash-report ping cleanup,
finish before workload preparation and warmup. The gate records identity
transitions, fails after 120 seconds, and does not disable crash reporting or
relax the measurement's process-turnover rule. Short smoke runs skip this gate.
Fresh-process comparisons must budget this settling time for every arm.

Warmup runs the same workload before measurement. Backend/geometry/library checks
and compositor screenshots occur outside the measured interval. The process
sampling interval brackets the Marionette call and is slightly wider than the
page's timer interval; both times are retained. Screenshot readback requests a
fresh render, so it does not independently establish partial-presentation or
unassisted restoration correctness. Those have separate focused tests.

`smoke` is limited to five seconds and validates the machinery. `timing` uses
light process sampling, `memory` additionally reads process memory rollups, and
`diagnostic` enables cumulative HAL render metrics for Vulkan without process
sampling. GL has no equivalent HAL counters, so the launcher rejects a GL
`diagnostic` phase. Metrics record attempted/recorded work, not GPU time.
Diagnostic records include startup and post-interval boundary
work; group by process/device/renderer and compare appropriate complete snapshots,
never add cumulative records. See [counter definitions](VulkanRendering.md).

Every process sample retains PID plus start-time identity, per-process CPU and
RSS, collector CPU and aggregate host CPU/load counters. Full samples
also retain FD counts, DRM client identities/raw counters, and available host
state. Memory
samples distinguish available PSS/private coverage from missing data. Primary
steady timing rejects process turnover because subtracting the totals of changing
process sets can lose CPU consumed by a departed child. Short-lived processes
between samples cannot be measured exactly; do not claim complete lifecycle CPU
accounting from this sampler. RSS sums double-count shared mappings; PSS is
reported separately. Raw DRM counters are not isolated GPU durations. Host
samples include aggregate CPU ticks/load, aggregate non-browser process CPU,
governor, platform profile, clocks, power and available temperatures, with
explicit unavailable envelopes. The aggregate background-process counter can
lose departed-process CPU, so use whole-host CPU ticks and saved process identity
evidence when investigating contention.

Timing collects full first/last samples and light intermediate samples at a
250 ms wait cadence by default. `--sample-interval` accepts 0.25–2 seconds for
timing sensitivity controls; other phases retain the default. The requested
interval is recorded in the configuration and checked against the sampler's
actual interval in the report. A longer interval observes process turnover less
frequently. Compare such controls separately from primary runs, retaining the
full endpoint checks, startup settling and all observed-identity gates.

Light samples discover browser descendants recursively
through every thread's Linux `/proc/PID/task/TID/children`, then read only the
browser tree and collector process statistics. They omit the aggregate
outside-browser process counter; whole-host CPU/load remain available. Full
samples retain the whole-host process scan. Missing child-discovery access fails
collection. As with other sampled process observations, this is not an atomic
snapshot: the [kernel children interface](https://docs.kernel.org/filesystems/proc.html#proc-pid-task-tid-children-information-about-task-children)
can miss children during process churn. Never infer complete lifecycle accounting
or the absence of short-lived processes from stable sampled identities.

Light samples also skip FD/fdinfo, memory rollups and sysfs
clock/temperature/power/governor/profile reads. Their omitted fields are explicitly
null or marked not collected; they are not zero measurements. Memory and smoke
retain full sampling. Endpoint-only hardware readings cannot establish conditions
throughout the interval. Collection runs outside the browser tree but can still
perturb the host; calibrate its overhead before primary timing.

Samples also identify the collector process and its CPU counters separately from
the browser tree. That process includes sampling and Marionette harness threads.
Per-sample thread CPU and wall costs cover collection itself. Missing collector
identity is explicit; it is never reported as zero cost. Sampling waits the
configured interval after each collection, so actual cadence includes collection
time. Use the
first/last `cpuSampleTimeSeconds` timestamps for new CPU-rate denominators; they
are captured immediately after the process-stat scan. Older reports only have
`timeSeconds`, captured after per-process collection. Do not use the wider host
call bracket. These reads are non-atomic and clock-tick quantized. Characterize
collection overhead separately; do not subtract it from browser or host results
as though its effect on scheduling and GPU contention were known.
New run configurations require valid, stable collector attribution outside the
Firefox process tree. Older schema-1 pilot reports remain readable without those
fields and cannot support collector-cost attribution. New timing configurations
also require full endpoints, light intermediate samples and valid CPU timestamps.

The runner records binary/fixture hashes, configuration, command, selected
environment and complete logs. The fixture records runtime library mappings
and hashes before/after measurement, including the exact mapped `libxul` and
loader/driver libraries. Supported viewports are at least 800 by 600 pixels so
every fixed workload checkpoint remains in bounds. There is no cold-driver-cache
control yet: a fresh profile alone is not a cold graphics pipeline or OS-cache experiment. Startup,
matched Wrench fixed-work measurements, lifecycle/memory plateau analysis and
paired-series orchestration require their own audited protocols before use.

## Offline checks

`test_renderer_benchmark_metrics.py` exercises malformed/incomplete reports,
wrong backend/software fallback, geometry changes, diagnostic availability,
process identity turnover and sampling failures without launching a browser.
`test_renderer_benchmark_startup.py` checks startup age, child turnover, PID reuse,
root replacement and bounded timeout with deterministic clocks.
Browser smoke results and implementation limits belong in the corresponding
Stage 9 run manifest; no historical result is a current-build pass.

The initial Stage 9.1 machinery check against the preserved `b7ba23c55b6`
Firefox runtime passed all 11 bounded cases: six Vulkan GPU-process workloads,
Vulkan diagnostics for static and CSS workloads, one Vulkan parent-process dirty
update, and GL static/CSS controls. Active cases completed 58–63 updates; all
backend, process, runtime-library, geometry and workload-pixel gates passed, with
no validation errors. These short Xvfb/Lavapipe and GL checks establish harness
operability only. They are not native hardware results or performance data. Raw
reports are under `artifacts/stage9/b7ba23c55b6/smoke/`.

## Native sampling sensitivity screen

The Stage 9 screen at harness revision `7ea9f8fa033` completed eight valid native
Intel runs using the preserved Firefox binary, GPU process, 890×705 viewport and
DPR 1. Each run used a fresh process/profile, startup settling, ten-second warmup
and a sixty-second interval. Sampling waits of 250 ms and two seconds were
compared once per workload/backend, with their order alternated between cases.

CPU values below are percentages of one core, calculated from first/last CPU
sample timestamps. Collector CPU is separate from Firefox CPU and is not
subtracted from it.

| Workload/backend | Firefox, 250 ms | Firefox, 2 s | Collector, 250 ms | Collector, 2 s |
| --- | ---: | ---: | ---: | ---: |
| Static GL | 0.83% | 0.85% | 4.53% | 0.65% |
| Static Vulkan | 0.98% | 0.75% | 4.48% | 0.63% |
| CSS GL | 17.71% | 18.94% | 5.08% | 0.78% |
| CSS Vulkan | 25.26% | 24.91% | 5.36% | 0.75% |

Two-second sampling reduced collector CPU by roughly 85%. CSS callback intervals
averaged about 16.666 ms at both frequencies; these are not GPU completion or
scanout measurements. Firefox CPU did not shift consistently with sampling
frequency: CSS GL increased about 7% with sparse sampling, while CSS Vulkan
decreased about 1.4%. With one observation per cell, this screen cannot separate
sampling effects from run variation or establish a backend performance result.
Lower collector cost does not prove negligible perturbation, and sparse samples
provide less process-turnover coverage.

Every raw run, the fixed order and analysis are retained under
`artifacts/stage9/7ea9f8fa033/step-9.4a/sampling-sensitivity/`. The full paired
series was not started by this screen.

Use a fixed two-second interval for the subsequent browser series to reduce
observer work, while retaining full endpoints and the sampled-lifecycle caveat.
This is a protocol choice, not a correction for a measured bias. Four balanced
pairs per workload must characterize run variability and backend deltas at that
single cadence; inconsistent paired directions remain inconclusive. The full
series also remains gated on separate Wrench-wrapper inspection overhead.

## Wrench post-result inspection

The local `artifacts/stage9/b7ba23c55b6/run_wrench_case.py` driver delegates
measurement readiness and live-capture ordering to
`wrench_benchmark_inspection.py`. Wrench creates its result file before rendering;
the driver waits for fully parsed JSON with the expected schema, completed flag
and inspection hold, plus the final PNG. The preserved Wrench measurement command
writes the PNG and completed JSON after its timed frames, then retains its
renderer and device during the requested hold.

Before readiness, the driver only polls the process and result file at 25 ms
intervals. It then takes two full process/FD/DRM/library-mapping captures during
the existing 500 ms hold. It records readiness and capture timestamps and checks
that the process remains alive before and after each capture. Missing readiness,
an invalid result, early exit, capture failure or timeout rejects the run.
The completed-result hash must still match after normal process exit. Library
hashing happens after exit. The separate PNG correctness command retains its
earlier inspection path because it has no measurement result or inspection hold.

The manifest labels these snapshots `post-result`. Their CPU counters do not
measure CPU consumed by the timed frames, and their host state does not describe
conditions throughout rendering. Device and library observations establish the
route during the retained post-result lifetime, not continuous monitoring of the
render interval. File/process polling still has a cost; this protocol removes
expensive inspection from the timed work without claiming zero observer overhead.

Validation passed eleven offline inspection tests and nine live correctness
checks against the preserved Wrench v4 binary. All eight measurement cases
captured exactly two post-result snapshots, kept the process alive across both
captures, and retained the same completed-result hash through exit. Both captures
finished 65–90 ms after readiness. Native GL/Vulkan pairs for alpha, deterministic
text and clip/blur matched PCI `0000:00:02.0` and produced identical PNGs.
Software GL/Vulkan under Xvfb and the legacy GL PNG path also passed. These
checks establish capture and routing correctness. Raw evidence and the reviewed
artifact driver are retained under
`artifacts/stage9/2fbacb7082c/step-9.4a/wrench-inspection/`.

## Native steady-state baseline

The full Stage 9.4a series at harness revision `4e8c88971fa` passed all 80 cases
without a retry in 125.76 minutes: eight browser preflights, 48 browser timing
arms and 24 Wrench arms. It used the preserved optimized development binaries,
native Intel hardware, the system loader and no validation or render diagnostics.
These results apply to this build, host and fixed fixture set.

Browser timing used four GL/Vulkan pairs per workload in AB/BA/AB/BA order,
fresh processes/profiles, startup settling, ten-second warmup, sixty-second
measurement and two-second sampling. CPU rates use the CPU sample anchor span.
The GL/Vulkan columns below are medians of run rates, expressed as percentages
of one core. Paired changes are medians and ranges of the four pair-relative
changes, rather than ratios of the column medians.

| Workload | GL CPU | Vulkan CPU | Median paired change | Paired range | Direction |
| --- | ---: | ---: | ---: | ---: | --- |
| Static | 1.02% | 0.97% | Inconclusive | -13.3% to +20.4% | Mixed |
| CSS transform | 19.46% | 26.19% | +34.8% | +28.1% to +43.0% | Higher in 4/4 |
| Small dirty update | 37.10% | 44.90% | +20.6% | +18.5% to +25.2% | Higher in 4/4 |
| Scroll | 31.65% | 36.81% | +17.4% | +14.6% to +20.5% | Higher in 4/4 |
| Software Canvas2D | 28.96% | 57.32% | +97.9% | +90.8% to +101.4% | Higher in 4/4 |
| Filters | 22.38% | 30.98% | +38.5% | +32.2% to +50.5% | Higher in 4/4 |

The extra CPU time is concentrated in the GPU process. For Canvas, its median
paired increase is about 0.282 CPU seconds per sampled second, while the median
increase for the entire Firefox tree is about 0.284. Process-level attribution
does not identify the responsible renderer or driver functions. Active workload
callback means remained near 16.666 ms, with per-run p99 values of 17.20–17.28 ms.
Callback timing does not measure physical scanout or GPU completion. Collector
CPU remained separate and was not subtracted from Firefox CPU.

Wrench used four pairs per fixture, 200 warmup frames and 5000 measured frames
per arm. The table compares each run's mean serialized frame roundtrip; the
GL/Vulkan columns are medians of those run means. Each roundtrip includes
scene readiness, the render call and synchronous completion readback. GL uses an
X11 backbuffer without swaps; Vulkan uses an owned offscreen texture.

| Fixture | GL roundtrip | Vulkan roundtrip | Median paired change | Paired range | Direction |
| --- | ---: | ---: | ---: | ---: | --- |
| Alpha/owned image | 1.615 ms | 1.849 ms | +14.5% | +0.8% to +20.0% | Higher in 4/4 |
| Deterministic text | 1.611 ms | 1.825 ms | +12.9% | +11.2% to +17.8% | Higher in 4/4 |
| Clip/blur | 1.900 ms | 1.957 ms | Inconclusive | -2.3% to +14.4% | Mixed |

Context/renderer initialization was observed separately before fixed-frame work.
Median GL/Vulkan values were 83.98/22.61 ms for alpha, 72.71/20.41 ms for text
and 71.05/22.25 ms for clip/blur. The routes differ: GL creates a native window
and backbuffer, while Vulkan uses an offscreen target. Driver and application
caches were not controlled as cold, so this does not support a general startup
claim or offset the steady-state roundtrips.

All twelve Wrench pairs matched PNG hashes and PCI identity. The post-result
process snapshots are route evidence, not frame CPU measurements. Whole-run
pairs are the independent observations; individual frames are not pooled as
independent runs. Four pairs provide a bounded local comparison rather than a
general performance guarantee. Static and clip/blur results remain inconclusive
because their paired directions differ.

The active browser CPU differences justify a focused GPU-process investigation,
starting with the software Canvas workload. Cold-start, lifecycle, memory and
separate diagnostics remain unfinished parts of Stage 9. Raw runs, the fixed
plan and incremental execution record are retained under
`artifacts/stage9/4e8c88971fa/step-9.4a/full-series/`.

## Linux perf investigation

`--phase profile` records the identified GPU process with an explicitly supplied
`--perf-binary`. It uses the same native hardware, preserved Firefox binary,
startup settling, viewport and Canvas producer policy as the timing harness.
Profiles are instrumented diagnostics and remain separate from primary timing.
For example, the local executable can be supplied as
`--perf-binary /usr/lib/linux-hwe-6.17-tools-6.17.0-42/perf`, with
the required two-second process sampling interval, selected by default in this
phase.

The recorder starts disabled after warmup, acknowledges a control ping, and
acknowledges enable/disable commands around the workload. It requests `cpu-clock:uk`
at 99 Hz with 16 KiB DWARF user stacks, monotonic timestamps and default thread
inheritance. `--perf-event cpu-clock:u` explicitly restricts a control capture to
user space. Recording is disabled before screenshot validation; final data
flushing and hashing happen after process sampling. No kernel policy or browser
sandbox setting is changed to obtain a profile.

The case directory is private, with mode-0600 control FIFOs. Evidence records the
exact perf binary/hash/version, event, command, GPU PID/start-time identity,
control request/acknowledgement times, exit status and data hash. A control
timeout, early recorder exit, changed target identity or failed finalization
rejects the capture. After flushing, the recorded event attributes must match
the requested user/kernel exclusions, frequency, inheritance, clock and stack
settings. Bare `cpu-clock` is not accepted because perf can silently restrict it
to user space under the host policy. Cleanup signals only the recorder's owned
process group.

Successful capture is distinct from a useful profile. Inspect `perf.data` with
the same perf binary after collection; retain sample counts by user/kernel mode,
thread and DSO, build IDs, lost records and unresolved or truncated stacks.
Preserved `libxul` and mapped-library paths support local symbol lookup; perf's
build-ID cache is not populated by the recorder. Report unresolved driver/kernel
symbols explicitly. Sampling weights describe the recorded on-CPU distribution
and do not establish isolated GPU duration or a performance improvement.

Recording uses `--buildid-mmap` so build IDs are embedded in MMAP2 records and
perf does not scan sampled call chains during shutdown. After clean exit, a
bounded symbol-free `perf script -G --show-mmap-events -F time --ns` pass retains
the MMAP2 IDs for the required pinned Firefox/libxul paths and derives actual
sample bounds. The IDs prove retention for those mappings; the harness does not
recalculate ELF-note IDs. Control acknowledgements have a ten-second timeout;
data finalization has its own recorded 120-second bound, included in the outer
harness budget. Both occur outside the primary timing methodology, and
finalization follows the sampled workload interval.

The first native GL profile completed its workload and control acknowledgements,
but exceeded the original ten-second shutdown bound while perf repeatedly called
`addr2line` for `libxul`. A second attempt used `--buildid-all`; it removed those
messages but still failed its separate 120-second finalization bound. Both data
files have incomplete headers and remain rejected. The all-DSO mapped-libxul
probe did not reproduce Firefox's mapping set and was insufficient evidence for
another comparison retry.

The Canvas baseline's GPU-process median user/system CPU rates were
0.16485/0.03044 CPU seconds per second for GL and 0.24012/0.23552 for Vulkan.
The larger system-time increase makes kernel-inclusive capture important;
user-only Rust or Mesa stacks cover only part of the observed difference.

The native tooling preflight confirmed user-space capture with the supplied perf
6.17.13 binary: the final explicit `cpu-clock:u` control produced 35 samples,
zero lost samples and resolved Python/libc call chains. An explicit
`cpu-clock:uk` control was initially restricted to user space and was correctly
rejected by the actual-attribute gate. The user subsequently enabled kernel
access and supplied a private symbol snapshot.

The corrected short native GL Firefox smoke then passed in 82.146 seconds. Its
actual event included user and kernel execution, tracking build-ID support was
enabled, required Firefox/libxul mapping IDs were retained, and 21 bounded samples
produced a 541,816-byte data file. Perf finalization took 0.720 seconds. This
validates capture machinery only; it is not a GL/Vulkan profile comparison. Both
counter controls remain valid, while four fresh Canvas profiles still require a
new quiet-host window and will not silently switch to user-only sampling. Probe
evidence is retained under
`artifacts/stage9/cff6f7717dc/step-9.5/perf-preflight/`; rejected browser attempts
remain under the adjacent Canvas investigation directories.
