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
250 ms wait cadence. Light samples discover browser descendants recursively
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
identity is explicit; it is never reported as zero cost. Sampling waits 250 ms
after each collection, so actual cadence includes collection time. Use the
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
