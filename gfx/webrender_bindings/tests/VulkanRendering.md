# Vulkan rendering diagnostics

HAL `PreparedFrameInfo` preserves WebRender's render and present decisions.
Readiness can coalesce notifications: a later no-render notification does not
erase an earlier unconsumed render request for that document, even when both
use the same frame publication ID. Once consumed, that request no longer makes
subsequent no-render notifications request rendering. This metadata alone does
not establish that the caller has retained valid output after resize or loss.

`render_if_needed()` returns `Rendered`, `Reused`, or `Skipped` for an offscreen
notification that needs no GPU work. Call `prepare_frame()` or
`prepare_frame_if_ready()` first: conditional rendering rejects an unconsumed
ready notification instead of drawing stale state. Per-document render requests
remain pending across later no-render notifications until a successful draw.
Legacy `render()` and `render_frame()` remain explicit render operations.

Reuse requires a current initialized owned output for the active document,
matching its dimensions, origin, clear color and surface generation. Required
offscreen work, forced redraw, document changes, invalid output and device loss
prevent reuse. Reuse still fulfills `FrameRendered` notifications; it does not
acquire, compose, submit or present. Wrench services every prepared frame through
this API and can re-present retained pixels for a genuine expose. Its explicit
`--hal-frames` and `--no-block` modes continue rendering repeated scenes.

Completion polling checks both renderer and external-image producer queues and
queued release callbacks. It stops once those queues drain. Firefox retains its
existing render decision path: its update-only frame completion does not submit
GPU work unless commands are pending, and does not present without an acquired
surface. These changes do not infer equivalence from DOM or display-list equality.

Stage 1b validation passed 167 ordinary tests and six focused Lavapipe reuse
tests. A private-Xvfb Wrench run with llvmpipe, the fixed Vulkan loader and
validation layers recorded no executions, full compositions, presentations or
device submissions during a fresh 3.2-second static interval. Resize and
minimize/restore preserved the expected red pixels, and `--hal-frames 3`
presented exactly three frames. Watch mode recorded 17 authoritative render
requests and executions for both an untouched interval and an identical file
rewrite before displaying the updated green pixels; those phases are controls
showing that reuse does not override WebRender's render decision. Artifacts are
under `artifacts/vulkan-idle/stage1b-wrench/`. This is functional llvmpipe
coverage and makes no hardware-performance claim.

Set `WR_HAL_RENDER_METRICS=1` before launching a HAL renderer to collect
aggregate rendering counters. It emits `WR HAL render metrics: {json}` at most
once per second during existing renderer polling, and a final record when the
metrics owner is destroyed. It creates no reporting timer or GPU wait. Without
the variable, metrics objects and reports are disabled.

Records use schema version 1 and identify the process, HAL device and renderer.
`rendererId: 0` reports shared-device activity; nonzero renderer IDs report
frame notifications, rendering, acquisition and presentation for a renderer.
Counters are cumulative, sequences are monotonic within each scope, and
timestamps share a process-local monotonic origin. `lastWorkNs` records the
latest increment of each counter, so an idle renderer needs no wakeup to report
that it has stopped working. Compare quiescent snapshots for exact deltas;
notification and render threads can update counters during a live snapshot.

Parsers must group records by `(pid, deviceId, rendererId)` and use the latest
complete cumulative snapshot by sequence within each scope, never sum snapshots.
Device and renderer scopes must remain separate. Records can be incomplete or
missing on abrupt exit or an output error; skip incomplete/nonmatching lines and
do not require a final record. Renderer/device final records can arrive separately
or be delayed by outstanding callbacks. Reports are best-effort and output errors
do not interrupt renderer shutdown.

The renderer counters distinguish render/no-render notifications, explicit
redraws, update/render wakeups, executions, picture-cache tile work, composition
area, swapchain acquisition/presentation and readbacks. These describe requests
and recorded work, not GPU execution time or display scanout. Failed frames can
contribute attempted rendering work without a successful presentation.

`acquires` and `presents` count successful operations; `discards` counts explicit
`FrameRenderer::discard_surface` calls that held an acquired image. Teardown
discard and failed presentation are not paired into those counters. Do not infer
the number of outstanding swapchain images by subtracting them.

Device `queueSubmissions` counts successful HAL `SubmissionQueue` submissions,
including the external-image producer queue. `surfaceSubmissions` is its subset
submitted with acquired surfaces. Synchronous bootstrap/probe commands outside
that queue are not counted. `pendingSubmissions` tracks submitted objects until
retirement or destruction; an abandoned-device cleanup is not proof of success.

`externalLeaseAcquires`, `externalLeaseReleases` and `externalLeases` track
renderer-attached plane leases and execution of their release callbacks. They
are not decoder-publication counts: multiple plane leases can share one image,
and final DMA-BUF ownership return may remain queued afterward. Check pending
submissions and the existing native lifetime tests as well.

Texture/buffer gauges are sampled from existing device accounting while polling;
their peaks are sampled peaks. Pending submission/lease peaks are updated on
the corresponding events. `retainedOutputBytes` describes the current final
RGBA output dimensions; it is not total allocated GPU memory. Resource-upload
counters cover the renderer's texture-cache uploads.

Browser validation must use actual Vulkan composition and compositor readback
when comparing pixels. A readback can request additional rendering: measure
idle counters before it and account for that boundary operation explicitly.
Use quiet runs without these counters for performance timing.

## P0 baseline

The opt-in fixture passed with metrics disabled and enabled using the Vulkan
`wgpu-hal` GPU process, llvmpipe from Mesa 26.2.2, the fixed Vulkan loader and
validation layers. Both runs produced the expected red, green and blue pixel
samples; the disabled run emitted no metric records. Raw reports and logs are in
`artifacts/vulkan-idle/p0-off-lavapipe/` and
`artifacts/vulkan-idle/p0-on-lavapipe/`.

In the enabled run, the steady `requestAnimationFrame` and repeated no-op style
phases each recorded one execution, full composition and presentation. The CSS
transform and paint phases each recorded 91 full compositions, while the WebGPU
phase recorded 30 full compositions, 89 queue submissions and 29 matched
external-lease acquire/release callbacks. Pending-submission and external-lease
gauges returned to zero. These phase deltas include the preceding checkpoint
boundary and queue draining because the poll snapshot is taken before the
current boundary draw. They are a functional baseline for later exact Rust
snapshot tests, not a zero-work assertion, isolated cause attribution or
hardware-performance result. The P0 windowed browser route could not initialize
the Intel ICD in that test environment, so the baseline used llvmpipe. Later
Intel offscreen validation succeeded; the P0 result does not establish that the
hardware device was absent.

## Retained partial composition

Vulkan Draw compositors can update an initialized owned final target using WebRender's
valid dirty rectangles. The initial supported subset requires the same document,
output dimensions/origin, clear color, surface generation and composition
descriptor, a newly built frame, and no deferred external images or external
compositor surfaces. Dirty regions are clamped and combined into one rectangle.
The renderer clears that rectangle and composites all intersecting contributors
in their original order. Pixels outside it remain untouched.

Unknown or invalid damage, an empty/full-size dirty union, forced redraw, changed
composition metadata, external images, and Native/Layer compositors use a full
composition. Full fallbacks report full output damage, including recomposition
of an already-rendered frame after external-image invalidation. Surface
presentation still copies the full output in this stage.

`WR_HAL_FORCE_FULL_COMPOSITION=1` disables partial composition for a comparison
run using the same binary. `partialCompositions` and `composedPixels` describe
final-target work; they do not include tile rasterization or presentation area.
Retained writes and previously requested readback copies share the renderer's
submission queue, so asynchronous readbacks retain their requested pixels
without adding a full-target copy or CPU wait before each partial update.

The retained-composition validation passed 167 ordinary tests, six focused
partial-composition tests and six retained-output tests using llvmpipe with
Vulkan validation. In the 2048 by 1024 queued-readback case, the retained
update composed 524,288 pixels, 25% of the 2,097,152-pixel full target. The
queued readback preserved the preceding output and the updated pixels matched
the forced-full control. Both renderers drained all pending submissions. This
is correctness and work-counter evidence from a software Vulkan device, not a
hardware-performance result. Detailed results are in
`artifacts/vulkan-idle-rendering/2a/partial-lavapipe-final-result.txt`.

## Preserved Vulkan swapchain images

Incremental presentation is enabled only for native Vulkan swapchains configured
with clipping disabled. The HAL requests this explicitly; other wgpu clients
keep their existing clipping default. Vulkan permits obscured swapchain pixels
to be undefined when clipping is enabled, so a stable image handle alone is
insufficient. See the [Vulkan swapchain configuration contract](https://docs.vulkan.org/refpages/latest/refpages/source/VkSwapchainCreateInfoKHR.html).

The renderer tracks the output version stored in each native `VkImage` and unions
damage from the output versions that image missed. It retains at most 32 changes
and 16 image versions. Unknown images, expired history, size changes, stale output
handles and unsupported surfaces use full updates. Reconfiguration, discard,
loss and suboptimal presentation invalidate image history. A known image with no
new damage still submits the acquire/present semaphore synchronization, without
copying pixels. Image contents are reused in the `PRESENT` layout only after a
successful tracked presentation.

`WR_HAL_FORCE_FULL_PRESENT=1` provides the full-presentation control while keeping
the same preservation configuration. It is independent of
`WR_HAL_FORCE_FULL_COMPOSITION`. `fullPresentUpdates`, `partialPresentUpdates`,
`unchangedPresentUpdates` and `presentPixels` count recorded update work, including
attempts that may fail later. `initializedSurfaceImages` samples the current
tracked image count during polling. Configuration logs and `SurfaceInfo` expose
`contents_preserved`; these are configuration diagnostics, not per-frame logs.
Presentation damage hints from `VK_KHR_incremental_present` are not required for
these GPU copy/draw savings and are not added here.

Vulkan presentation preserves the tracked image contents and does not itself
modify them. This history therefore applies to a window rendered exclusively
through this Vulkan swapchain. Any non-Vulkan client pixel write to the platform
window invalidates the tracked contents and requires a fresh full render before
the next presentation.

The native X11 validation passed with both blit and draw presentation routes,
their forced-full controls, three rotating swapchain images, resize and injected
surface loss. The incremental runs recorded 18,270,048 presented pixels instead
of 44,042,912, a 58.52% reduction in recorded presentation area, while saving
25,772,864 pixel updates. Six synthetic expose checks caused no render
executions. All 36 sampled pixels matched, queues drained and Vulkan validation
reported no error. These samples do not prove full-window pixel equivalence and
the area counters are not a timing result. The test used llvmpipe, so it makes
no hardware-performance claim. The report is in
`artifacts/vulkan-idle/stage2b-wrench/report.json`.

## Native X11 visibility

The Vulkan compositor's native visibility query distinguishes unmapped or
unviewable windows and the WM's `_NET_WM_STATE_HIDDEN` property from visible and
unknown states. An unmapped ancestor is reflected in the client window's map
state. Focus and mere coverage by another window do not determine hidden state.
Workspace hiding is recognized when the WM reports it through these signals;
unsupported or unknown states remain renderable.

Checked XCB replies use the existing X11 connection without replacing Xlib's
error handler, taking events away from GTK, or opening another connection.
Attribute and property requests are batched into one round trip after atom
initialization. Missing properties on a viewable window mean visible; malformed
or truncated properties never establish visibility. Failed queries mean unknown.
Queries run only when requested by the renderer, with no visibility timer.
Returning from hidden to visible or unknown invalidates output for a full refresh.
Native visibility remains independent of explicit renderer Pause/Resume.

The native X11 visibility suite passed all eight cases on a private Xvfb
display, with no skipped or failed tests. It covered map state, an unmapped
ancestor, focus independence, hidden and missing WM properties, malformed and
truncated properties, preservation of an Xlib-queued event, and a destroyed
window returning unknown without a fatal X error. The report is in
`artifacts/vulkan-idle/stage3a-x11/report.json`.

## Hidden-window servicing

For a healthy, unpaused Vulkan renderer, a reliably hidden native window skips
swapchain acquisition, final window composition and presentation. Renderer
messages, pipeline updates, `FrameRendered` notifications and frame-completion
IDs still advance. Required texture-cache/offscreen passes execute, and existing
completion polling drains submitted work and external-image releases. No polling
is added once the queues are idle, and no browser animation or scheduling policy
is changed. Each window has independent visibility state.

Explicit readback, profiler screenshots and composition recording can still
render a fresh owned final target while hidden, without acquiring or presenting
a window surface. Existing empty-size capture failure and lifecycle/device pause
handling remain in effect. Pending screenshot queues continue to be serviced.
Restoration forces a full refresh from WebRender's latest state rather than
replaying hidden frames.

`hiddenSkips` counts serviced hidden requests. It is separate from actual
`offscreenExecutions`, `fullCompositions`, `partialCompositions`, `acquires` and
`presents`; explicit hidden captures legitimately increase composition/readback
counts. Legacy embedding frame accounting still sees a serviced request, so use
the HAL work counters when distinguishing logical frames from GPU rendering.

The focused Intel offscreen validation passed all 18 cases: five hidden-frame,
six partial-composition, six retained-output and one draw-presentation test. It
used the Intel ICD, native SPIR-V and the fixed Vulkan loader with validation;
the only driver message was the expected experimental Xe KMD warning. This is
offscreen correctness coverage and does not establish native-window behavior or
hardware performance. Detailed results are in
`artifacts/vulkan-idle-rendering/3b/intel-offscreen-result.txt`.

`WR_HAL_FORCE_VISIBLE=1` disables the native hidden-window optimization for a
same-binary control. It does not map, focus or change the native window, and
existing lifecycle/device pause checks still apply.

The private-X11 browser workspace test passed with both GPU-process and
parent-process WebRender. In each run, three fresh hidden requests advanced
updates and `hiddenSkips` without executions, compositions, readbacks, surface
acquires or presents, while another window presented three frames. A hidden
capture then added one readback and one full composition with no surface work,
returned the expected 890 by 617 red image, and restoration displayed the newer
green state through the native X window. Pending submissions and leases drained
to zero. Resizing the hidden client to 760 by 620 performed only required
offscreen work, with no final composition or surface operation. Two further
hide/restore cycles in each process mode again serviced hidden requests without
screen work and restored the exact current native pixel. The other window
continued presenting and its animation advanced throughout the test.

The minimized-window diagnostic produced the same result with the optimization
enabled and with `WR_HAL_FORCE_VISIBLE=1`: no main-window HAL frame or readback
work was recorded, and both 890 by 617 WebDriver captures were transparent. The
other window remained active and native restoration displayed the expected
green state. This records unchanged minimized-capture behavior relative to the
new optimization; it does not attribute the behavior to browser internals.
Reports are under `artifacts/vulkan-idle-rendering/3b/` in
`browser-workspace-gpu/`, `browser-workspace-parent/`,
`browser-iconify-gpu-opt/` and `browser-iconify-gpu-control/`.

Browser interoperability coverage remained limited by this host setup. The
Lavapipe WebGPU control rendered correct Vulkan pixels but could not produce the
required direct DMA-BUF import marker under Xvfb. The WebGL fixture rejected its
software producer, and the Intel windowed Vulkan route could not initialize.
These unavailable gates are not pixel regressions, and the successful Intel
offscreen tests do not replace native-window interoperability coverage. None of
these correctness results is a timing measurement.
