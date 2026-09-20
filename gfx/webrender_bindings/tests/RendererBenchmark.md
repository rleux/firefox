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

Process samples retain PID plus start-time identity, per-process CPU and RSS,
FD counts, DRM client identities/raw counters, and available host state. Memory
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

The sampler scans browser-process fdinfo at 250 ms cadence to retain DRM and FD
coverage. That work runs outside the measured browser tree but can still perturb
the host. The pilot must compare instrumentation sensitivity before this is used
for primary timing; a successful smoke only establishes schema and operability.

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
