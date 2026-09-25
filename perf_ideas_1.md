# WebRender Vulkan performance ideas

Prioritized from source inspection. These are hypotheses, not measured wins.

1. **Track scroll damage to allow partial composition.** Scrolling marks composite tile dirty rectangles invalid, which prevents retained-output partial composition and causes the final composition to cover the full frame. Correct scroll-damage tracking could reduce composed pixels; validate correctness carefully. See `gfx/wr/webrender/src/composite.rs` (`dirty_rects_are_valid`) and `gfx/wr/webrender/src/renderer/hal.rs` (`execute_frame`).
2. **Normalize opaque upload alpha while packing staging data.** A slow path clones the full source image and rewrites alpha across the descriptor, even for a subrectangle upload. Doing this during staging packing could avoid a full-image allocation and pass. See `gfx/wr/webrender/src/device/hal/resources.rs` (`upload_recorded_with_alpha`).
3. **Measure frame-table upload frequency and cost.** The active `HalGpuBackend` keeps CPU mirrors and uploads storage buffers when the table buffer is absent, or texture data when the cached texture is stale. Measure table sizes, changed bytes, and CPU preparation before considering dirty-range or versioned updates. See `gfx/wr/webrender/src/device/hal/render/gpu_backend/tables.rs` (`bind_tables`).
4. **Reuse CPU scratch storage for instance packing.** Instance sizes, ranges, and packed uploads are prepared per draw pass; reusable scratch storage could reduce allocator churn without removing required packing or transfer. See `gfx/wr/webrender/src/device/hal/render.rs` (`upload_instances`).
5. **Use best-fit upload-buffer reuse.** The upload pool takes the first idle buffer with matching usage and sufficient capacity. Best-fit selection may avoid consuming a large buffer for a small upload, but the expected gain is lower priority. See `gfx/wr/webrender/src/device/hal/pool.rs` (`BufferPool::upload_with`).

## Scroll-damage implementation and browser measurement

The scroll-damage experiment was implemented and then reverted after the browser workload showed no measurable improvement. Its translation-only damage path had conservative fallbacks for changed surface identity, scale changes, external surfaces, deferred resolves, and tile rerasterization.

A local Firefox/Browsertime run used 240 `requestAnimationFrame`-paced scroll updates in a 520 × 520 nested scroller, with three iterations before and after:

| Measure | Before | After |
| --- | ---: | ---: |
| Scroll duration median | 3,982 ms | 3,982 ms |
| Gecko CPU time median | 4,010 ms | 4,011 ms |

This refresh-paced workload showed no measurable performance improvement. It does not establish that all scroll damage cases are accelerated; Browsertime did not capture the renderer's partial-composition counters.

## Opaque-upload experiment and browser comparison

The experiment normalized opaque alpha directly into the staging upload, avoiding the full-buffer clone and rewrite in the slow reinterpreted-layout path. That implementation has since been reverted; only the comparison workload remains.

The comparison fixture now has a `canvas-partial` workload. It initializes an opaque 512 × 512 canvas, then changes a centered 256 × 256 region on each frame. The local Vulkan run passed its pixel checks on Intel Iris Xe/Mesa 26.2.3. Three 10-second runs before and after produced these medians:

| Measure | Before | After |
| --- | ---: | ---: |
| GPU-process CPU time | 3.29 s | 3.21 s |
| Total Firefox CPU time | 4.48 s | 4.38 s |
| Canvas frame interval | 17.06 ms | 17.06 ms |

GPU-process CPU was about 2.4% lower and total Firefox CPU about 2.2% lower, with unchanged frame pacing. The run-to-run spread and frame count variation make this a directional result.

The `canvas-partial` fixture exercises opaque partial canvas updates, but does not prove that the slow reinterpreted-layout branch ran. The browser result therefore cannot be attributed confidently to avoiding the full-buffer clone. Reports are under `artifacts/opaque-partial-before-*` and `artifacts/opaque-partial-after-*`.

## Frame-table upload measurements

Added per-table counters to opt-in `WR_HAL_RENDER_METRICS` records from the active shared-renderer path. Each table reports source bytes, bytes written to mapped upload storage or texture staging, bytes changed through `upload_table`, CPU preparation time, and storage-buffer/data-texture upload counts. `prepareNs` is CPU call time, not GPU completion; `updatedBytes` sums update rectangles and can count overlapping regions more than once.

The diagnostic window from first to last renderer metrics record spans about 19 seconds on Iris Xe/Vulkan. Each full table set is four 32 KiB tables plus two 128 KiB GPU buffers. All samples used storage buffers:

| Workload | Renderer executions | Uploads per table | Bytes written | Updated bytes | CPU preparation |
| --- | ---: | ---: | ---: | ---: | ---: |
| `canvas-partial` | 1,006 / 972 | 15–31 | 5.6–11.6 MiB | 0.50–1.04 MiB | 0.88–1.96 ms |
| `dirty` | 976 | 973 | 364.9 MiB | 30.6 MiB | 74 ms |
| `css` | 951–969 | 948–961 | 355.5–360.3 MiB | 29.8–30.2 MiB | 95–138 ms |

The dirty and CSS runs upload every table on nearly every execution, but only about 8.4% of written bytes correspond to `upload_table` update rectangles. The canvas workload uploads tables much less often. CPU preparation time was small overall and varied between runs, so dirty-range updates need a focused follow-up before implementation. Reports are under `artifacts/frame-tables-diagnostic-2`, `artifacts/frame-tables-diagnostic-final`, `artifacts/frame-tables-dirty`, `artifacts/frame-tables-css`, and `artifacts/frame-tables-css-2`.

## Instance-packing scratch reuse

`FrameRenderer` now retains the CPU vectors used to collect packed instance sizes, lay out instance ranges, and size GPU upload buffers. `upload_instances` clears and refills those vectors per pass, and retains the `Rc` buffer vector capacity while releasing the per-pass buffer references after command recording. This removes repeated vector allocations once the renderer has reached its steady-state capacity; instance packing and GPU uploads are unchanged. The layout unit test now checks that empty/error reuse preserves the scratch vector capacities.

The optimized Rust build succeeded. Three 10-second `dirty` Vulkan browser runs before and after on Iris Xe/Mesa 26.2.3 all passed the benchmark checks. The process snapshots show no measurable change in the medians: GPU-process CPU was 1.73 s before and 1.75 s after; Firefox-root CPU was 4.11 s before and 4.14 s after; combined CPU for those processes was 5.84 s before and 5.86 s after. Median frame interval was 17.06 ms before and 17.04 ms after. One baseline run had a higher GPU-process reading (3.19 s), indicating run-to-run noise. The workload remained refresh-paced, and this measurement does not show a user-visible performance gain. Reports are under `artifacts/instance-before-1` through `artifacts/instance-before-3` and `artifacts/instance-after-4` through `artifacts/instance-after-6`.

## Best-fit upload-buffer reuse

`BufferPool::upload_with` now scans idle, uniquely owned buffers with matching usage and picks the smallest buffer large enough for the request. The pool is capped at 256 entries, so the additional scan is bounded. Buffers still remain unavailable while submissions hold references, and the existing byte budget and eviction rules are unchanged.

The optimized Rust build succeeded. Three 10-second `dirty` Vulkan browser runs before and after on Iris Xe/Mesa 26.2.3 passed the benchmark checks. Median frame interval was 17.06 ms both before and after, with 600 updates in each run. Process CPU readings varied substantially between runs: median GPU-process CPU was 2.37 s before and 1.82 s after, while median Firefox-root CPU was 4.58 s before and 4.23 s after. The spread, including a 3.21 s baseline and 3.83 s post-change GPU-process sample, is too large to attribute these differences to buffer selection. This refresh-paced workload shows no frame-pacing gain and does not establish a CPU improvement. One post-change attempt failed fixture setup because the window position differed; it is excluded. Reports are under `artifacts/idea5-before-1` through `artifacts/idea5-before-3` and `artifacts/idea5-after-2` through `artifacts/idea5-after-4`.

## Frame-table upload measurements

Added per-table counters to opt-in `WR_HAL_RENDER_METRICS` records from the active shared-renderer path. Each table reports source bytes, bytes written to mapped upload storage, bytes changed through `upload_table`, CPU preparation time, and storage-buffer/data-texture upload counts. `prepareNs` measures CPU call time, not GPU completion; `updatedBytes` sums updated rectangles and can count overlapping regions more than once.

On the same Iris Xe Vulkan setup, the diagnostic window from the first to last renderer metrics record covered about 19 seconds:

| Workload | Renderer executions | Uploads per table | Write bytes | Updated bytes | CPU preparation |
| --- | ---: | ---: | ---: | ---: | ---: |
| `canvas-partial` | 1,006 | 31 | 11.6 MiB | 1.04 MiB | 1.96 ms |
| `dirty` | 976 | 973 | 364.9 MiB | 30.6 MiB | 74 ms |
| `css` | 969 | 961 | 360.3 MiB | 30.2 MiB | 138 ms |

All runs used storage buffers. A second CSS run recorded 951 executions, 948 uploads per table, 355.5 MiB written, 29.8 MiB updated, and 95 ms of preparation time, showing noticeable timing variation. Each upload set contains four 32 KiB tables and two 128 KiB GPU buffers. The content-changing workloads rewrite each table on nearly every execution, while only about 8.4% of written bytes correspond to table update rectangles. This identifies a possible dirty-range opportunity, but the measured CPU preparation cost is small and noisy. Reports are under `artifacts/frame-tables-diagnostic-2`, `artifacts/frame-tables-dirty`, `artifacts/frame-tables-css`, and `artifacts/frame-tables-css-2`.
