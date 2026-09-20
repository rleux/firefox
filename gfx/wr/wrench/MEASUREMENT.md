# Fixed-work renderer measurements

The `measure` command runs a fixed YAML scene through the GL or HAL renderer
without swapping or presenting. Each sample rebuilds/submits the scene, waits
for a frame-ready notification, explicitly renders it, and completes a one-pixel
synchronous readback. A final full PNG readback is outside the sampled interval.
Existing `perf` behavior and its GL profiler queries are unchanged.

This measures serialized scene-to-readback wall time, including the explicit
completion readback. It is not an isolated GPU timer, a production asynchronous
throughput measurement, or browser frame cadence. `sceneReadyNs`, `renderCallNs`
and `completionReadbackNs` are consecutive wall-time components of `totalNs`.
The first component includes scene submission and backend readiness; the render
call can itself wait. No CPU/GPU overlap or engine-time inference follows from
these fields. All raw samples are retained without trimming tails.

The notifier accepts only `new_frame_ready` and shutdown notifications. Generic
update wakeups cannot satisfy the next sample. Unexpected additional frame
notifications fail the run. `renderRequested` records WR's decision, but this
explicit fixed-work operation renders every measured frame, including an
unchanged scene. It does not exercise the conditional idle API.

For a same-device hardware comparison, GL uses a window-backed context and its
backbuffer without swapping; Vulkan uses an owned offscreen texture. The default
GL headless route uses OSMesa and cannot establish hardware GL cost. Merely using
one host does not prove that the two APIs selected the same GPU: record and check
the actual renderer/driver plus native device identity externally. Both commands
must have identical size, input/assets, subpixel policy and shader settings.
Native compositor modes and the profiler overlay are rejected.

Example short operability commands, run from `gfx/wr/wrench` with an explicit
path to the chosen optimized binary:

```sh
WRENCH_BINARY --backend gl --size 257x129 --no-subpixel-aa \
  measure reftests/hal/alpha-depth.yaml /absolute/output/gl.json \
  --frames 5 --warmup 2

WRENCH_BINARY --backend hal --hal-backend vulkan --headless \
  --size 257x129 --no-subpixel-aa \
  measure reftests/hal/alpha-depth.yaml /absolute/output/vulkan.json \
  --frames 5 --warmup 2
```

`WRENCH_BINARY` is a placeholder for the executable path. Output directories must
already exist. Existing JSON/PNG paths are rejected. `--frames` must be positive;
both frame counts are bounded at 100000. A frame-ready wait times out after 30
seconds; an outer process timeout is still required for driver stalls and teardown.

Use private Xvfb for software correctness controls. Hardware comparison requires
a working native GL context and explicit Vulkan device selection. No presentation
calls occur in the command; native window/context initialization is still part
of the GL setup. `contextAndRendererInitializationNs` reports that setup interval
separately from scene samples. This is not isolated shader/pipeline compilation
time: fixture-dependent pipeline creation can occur in the first frame.

Before sustained measurements, establish pixel correctness with selected known-good
fixtures, verify actual adapters, obtain host-readiness confirmation, freeze the
optimized binary and record its hash, source/features, environment and input/asset
identities. Keep validation layers and detailed diagnostics out of primary timing.
Use balanced repeated pairs and retain invalid runs with their rejection reason.

For first-use measurements, `--warmup 0 --frames 1` retains the first frame. A
fresh process, application cache, driver shader cache and OS cache are different
conditions. A launcher must explicitly create separate driver-cache directories
when testing that cache, and must never remove the user's global cache. A new
process alone is not proof of a cold shader cache. Warm cases use the same scene
for all warmup and measured frames.

Inspect the final PNGs before comparing timings; the command saves output but
does not certify equality across backends or replace deferred Wrench acceptance.
Record unsupported scenes/driver routes as such. Do not widen reference tolerances
or silently fall back to another API to obtain a successful measurement.

## Initial correctness checks

The five-frame, two-warmup `alpha-depth.yaml` check passed with window-backed GL
under private Xvfb and offscreen Lavapipe Vulkan. Both reported zero presentations
and produced byte-identical PNGs. Sample counts and consecutive timing components
were checked; these short runs establish operability, not a performance result.
Zero-frame requests and existing output paths were rejected.

Validation exposed a pre-existing windowed GL teardown failure: the preserved
baseline's `png` command wrote the expected image and then failed with
`GLXBadWindow`. Keeping the native window alive until its GL context and drawable
are destroyed fixed the failure. Both the new measurement command and the existing
windowed `png` command then exited successfully with unchanged pixels. Evidence
is retained under `artifacts/stage9/b7ba23c55b6/wrench-smoke/`.
