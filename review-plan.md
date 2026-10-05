Vulkan3 review response plan
===========================

Treat every comment in `review.md` as a requirement. The resulting implementation must share its ordinary rendering machinery through wgpu-hal's dynamic interfaces, issue native render passes at WebRender pass boundaries, reuse bindings, avoid per-draw attachment barriers, and contain no branch-local modifications to vendored wgpu code. The Vulkan-specific AA workaround must leave the accepted implementation.

The numbered requirements below record the agreed plan. The execution record and commit map at the end distinguish completed work from validation that remains unavailable.

1. **Branch ownership and preservation**

   Implementation belongs exclusively on `vulkan3`. Keep `vulkan2` read-only: no commits, resets, rebases, formatting, builds, tests, or other validation on that branch.

   At analysis time, `vulkan3` and `vulkan2` both point to `638c03f6eb8400d6c8e29a8a623a982521f03c86`. The stack contains 118 commits above base `cfd140a311d8523d3d35e00ee1a9f2403c033dfd`, which is the current local `main`. The locally recorded `origin/main` is older than this base; that tracking reference is not evidence that this stack needs rebasing. Updating the base is outside this review response.

   A separate branch named `rejected` has been created at `638c03f6eb8400d6c8e29a8a623a982521f03c86`. It preserves the complete pre-cleanup implementation and its original commit boundaries, including the rejected material identified below. No checkout, build, test, lint, shader compilation, benchmark, or validation is to run on `rejected`. It is an archive, not another maintained implementation.

   Before removing code, record its original commit and paths in the removal ledger on `vulkan3`. The existing archive already preserves everything present in the starting stack, including the old versions of code that will be replaced. If subsequent work introduces anything that is later rejected, preserve that material on `rejected` before removing it from `vulkan3`, without validating the archive. Do not overwrite or delete the archive to simplify history.

   Keep the existing untracked `review.md`, `task.md`, `glsl-to-spirv.md`, and `vulkan-upload-performance-ideas.md` intact. Implementation decisions below follow `review.md`; they do not depend on treating the other notes as additional requirements.

   Preserve `vulkan2`'s existing commit split, logical grouping, and relative order in the rewritten `vulkan3` stack, except where the review changes require a different boundary or dependency order. The 118-commit stack is the structural starting point; the implementation stages below are work and validation checkpoints, not a replacement stack of six large commits.

   Apply these rules to the final history:

   - Fold each modification into the corresponding original logical commit on `vulkan3`, keeping its focused responsibility and associated tests. Do not leave the original rejected implementation followed by a large corrective tail.
   - Drop wholly rejected commits from `vulkan3` after preserving them on `rejected`. For mixed commits, retain the accepted responsibility and extract the rejected portion; move still-needed prerequisites into the earliest appropriate retained commit.
   - Preserve the existing `WR Vulkan/NN` group identities and within-group organization wherever their responsibilities survive. Update subjects and descriptions to explain the final implementation, including the shared HAL architecture. If an entire group disappears, leave its identifier unused rather than renumbering unrelated groups just to close gaps.
   - Split, add, merge, or reorder commits only when required to keep a review-driven change coherent, dependencies available, and each retained commit independently reviewable. Prefer a small prerequisite commit within the relevant existing group over merging distinct groups. Record the reason for every structural exception.
   - Maintain an old-to-new commit map on `vulkan3`: original hash/group, retained/modified/split/dropped status, replacement hash or hashes, and the review requirement explaining any boundary or order change. Commit hashes and the final commit count will change; preserving the split means preserving logical responsibilities, not retaining old hashes or empty commits.

   In particular, fold HAL resource ownership into the existing device/resource groups, pass and binding changes into their existing draw/program/render-pass groups, native integration changes into their existing platform/external-image groups, and documentation into its existing group. Removing the dependency-alignment, optional Naga, WebGPU vendor-dependent, or AA-workaround commits must not collapse unaffected groups.

2. **Review findings and required outcomes**

   | Review requirement | Observed implementation | Required outcome |
   | --- | --- | --- |
   | Share rendering machinery with future D3D12 and Metal backends | `Device` owns `OpenDevice<api::Vulkan>`; `Owned<T>` destroys through `vulkan::Device`; buffers, textures, pipelines, encoders, fences, and attachment views use concrete Vulkan types | Shared resource and rendering code uses `Dyn*` HAL interfaces; native extensions stay behind a narrow boundary |
   | Remove the vendored wgpu wrapper | `78cb94f83be` adds `gfx/wr/third_party/wgpu` | Remove the added wrapper and its workspace wiring |
   | Remove forced wrshell version alignment | `60b776f3a4f` changes wrshell, egui dependencies, workspace patches, Cargo configuration, and a vendored `wgpu-types` compatibility method | Restore wrshell's independent dependency choice and remove the associated compatibility patch |
   | Match native passes to WR passes | `RenderDevice::draw_instanced` calls `DrawPass::record` with one draw; `record_pass` begins and ends a native pass for every call | Collect and encode each WR pass together, with real attachment operations |
   | Reuse bindings | `ResolvedProgram::bindings` uploads projection data and calls `DrawBindings::new` for each draw; `record_batches` also allocates bindings per batch | Reuse layouts, descriptor groups, and upload allocations across compatible draws |
   | Remove per-draw attachment barriers | `Texture::transition` forces barriers for color/depth writes; `SurfaceView` has the equivalent color-target rule | Transition resources around actual passes and real hazards, not each draw |
   | Remove the backend-specific AA workaround | `7a40b8834ec` adds shader-name detection, packed-instance rewriting, and `VULKAN_AA_GRID` | Remove the workaround; diagnose and fix any necessary geometry issue in shared renderer code |
   | No local vendor patches | Changes touch Naga, wgpu-core, checksums, and wgpu-types | Restore the base vendor contents or consume an upstream revision containing accepted changes |

   The pass code does sometimes use `LOAD_CLEAR` for uninitialized attachments. That does not resolve the review: ordinary draws still create separate passes, stores are unconditional, and WR's declared operations do not consistently become native attachment operations.

3. **Remove rejected dependency additions and restore independent wrshell dependencies**

   Remove the effects of `78cb94f83be` and `60b776f3a4f` from `vulkan3`, repairing later dependencies in the same coherent change. Do not blindly restore the entire current workspace manifest or lockfile: later commits add legitimate HAL and shader-build dependencies.

   Concrete work:

   - Remove `gfx/wr/third_party/wgpu`, its workspace exclusion, and the `wgpu` path patch.
   - Restore wrshell's original dependency versions and corresponding GUI API usage. Remove the wrshell-specific optional Vulkan GUI wiring introduced by the rejected upgrade; this is separate from keeping Vulkan support in Wrench and WebRender.
   - Remove the branch-added `TextureFormat::is_srgb` alias and restore the matching `wgpu-types` checksum. Do not leave this vendor edit behind just because the review explicitly named only wgpu-core and Naga.
   - Reconstruct only the Cargo source replacement and patches that retained WebRender HAL dependencies require. `gfx/wr/.cargo/config.toml` was introduced by the rejected wrshell commit but also affects later HAL resolution, so its useful responsibility must be accounted for explicitly.
   - Update `gfx/wr/Cargo.lock` and the root lockfile only as necessary for the retained graphs. Keep wrshell's wgpu version independent of Firefox's HAL revision.
   - Remove added `pp-rs` and `unicode-xid` vendoring only if their only remaining reason for inclusion was the rejected optional Naga path. Resolve that question from the final graph on `vulkan3`.

   Firefox already needs wgpu-core and Naga for WebGPU. The review does not require deleting those existing dependencies. The new WebRender backend should directly depend on HAL and the types/build dependencies it needs, without adding the high-level wgpu wrapper or forcing unrelated consumers onto its version.

   Acceptance: wrshell has no forced upgrade attributable to this stack; the added wrapper is absent; retained HAL dependencies resolve independently; no associated wgpu-types source/checksum changes remain.

4. **Remove local vendor patches together with their dependent behavior**

   Use the following ledger to keep removals complete and recoverable. All original changes are preserved in `rejected`.

   | Original commit | Rejected material | Dependent work to address on `vulkan3` |
   | --- | --- | --- |
   | `78cb94f83be` | Vendored high-level wgpu wrapper | Workspace manifest and lockfile entries |
   | `60b776f3a4f` | Wrshell alignment and `wgpu-types` alias | Wrshell GUI API edits, optional GUI backend wiring, Cargo source configuration |
   | `b4f1b9b902a` | Local Naga GLSL/SPIR-V changes | Optional Naga compiler selection, feature forwarding, tests, dependency additions, and documentation |
   | `8f489c3114b` | wgpu-core imported-texture initialization changes | WebGPU DMA-BUF import that creates uncleared wrapped textures |
   | `5e72aebf05b` | wgpu-core initialization added to resource transitions | Publication code that relies on those transitions to initialize data before external use; split this mixed commit rather than treating all its integration as a vendor patch |
   | `368aff5f18a` | wgpu-core timestamp shader workaround | Determine whether any retained behavior still needs an upstream fix after restoring Naga |
   | `7a40b8834ec` | Vulkan-specific quad instance expansion and shader variant | Draw/upload paths, VAO bookkeeping, shader defines, and workaround-specific tests |

   Keep the existing glslang/SPIRV-Tools path as the initial supported Vulkan shader compiler. Remove the patch-dependent optional Naga mode, including stale configure choices and feature forwarding in WebRender, Gecko bindings, and toolkit Cargo manifests. Retain compiler-independent reflection or tests when they remain useful and work with unmodified dependencies. Do not discard all shader build infrastructure merely because it was touched by the Naga commit.

   For WebGPU sharing, the initial accepted implementation should defer the producer import/publication path that requires modified wgpu-core. Audit the WebGPU-specific Vulkan/20 and Vulkan/21 commits transitively: Rust FFI, `SharedTextureVulkan`, `WebGPUParent`, canvas presentation, IPC, allocation reuse, capability admission, migration, and tests. Remove dependent entry points and feature claims coherently instead of leaving a selectable but broken fast path.

   Preserve independently useful WR DMA-BUF consumption, foreign RGB/GL publication, external-image lifetime tracking, and surface lifecycle work. Shared consumers and synchronization infrastructure are not automatically rejected merely because the WebGPU producer uses them.

   Route WebGPU canvas presentation through the existing supported readback/CPU-buffer path. Follow its complete path into the Vulkan renderer before claiming it remains functional. Never pass `cleared = true` for uninitialized memory or remove an initialization obligation to make an import succeed. If a dependency-free implementation can be made with the existing upstream API, it can replace the deferred producer path as a separate justified change; it is not a prerequisite for the core HAL/pass refactor.

   Prepare distinct upstream proposals for genuinely needed Naga and wgpu-core fixes. Preserve their reproductions and rationale in the archive/ledger. No external submission is part of this planning task. Reintroduction requires upstream acceptance, a normal dependency update, and fresh validation on `vulkan3`; hiding local patches behind feature flags does not satisfy the review.

   Acceptance: no branch-local source or checksum edits remain under the affected vendor crates; retained capabilities work without them; deferred capability claims and selectors are removed.

5. **Extract a shared HAL backend and isolate native Vulkan integration**

   Proposed organization: put ordinary rendering code under a neutral module such as `device/wgpu/`, with a `vulkan` integration submodule. Keep public Vulkan startup selection where appropriate while making the core reusable by another HAL implementation. Directory naming alone is not an acceptance criterion.

   Migrate in dependency order:

   - Device/context ownership: use `DynOpenDevice`, `DynDevice`, `DynQueue`, and dynamic adapter/instance interfaces wherever no native extension is required.
   - Resource ownership: store `Box<dyn DynBuffer>`, `Box<dyn DynTexture>`, `Box<dyn DynTextureView>`, samplers, shaders, layouts, pipelines, bind groups, and fences. Keep WR metadata wrappers where needed for state, ownership, initialization, pooling, and lifetime tracking.
   - Destruction: replace `unsafe fn(&vulkan::Device, T)` with destruction through the owning dynamic device. Preserve explicit HAL destruction and ordering; dropping a boxed handle must not silently replace the resource's destruction contract.
   - Submission: convert command encoders, command buffers, queue submission, fences, recycling, retained resources, and rollback/commit behavior. Preserve device identity checks and prevent handles from different devices/backends reaching HAL calls together.
   - Rendering: migrate texture/buffer operations, pools, uploads/readback, pipelines, bindings, program state, vertex arrays, and pass encoding to the dynamic API.
   - Presentation: share configuration/acquire/present policy through `DynSurface` where supported. Isolate platform handles and native requirements. Keep borrowed swapchain-image ownership distinct from owned textures.

   Retain raw Vulkan operations only in explicit integration code: extension negotiation, DMA-BUF import/export, timeline/sync-file synchronization, external ownership transfers, native format/modifier queries, and any presentation operation HAL cannot express. Use checked downcasts there, with unsupported-operation errors for another backend. Common code must not acquire raw Vulkan handles to draw, allocate ordinary resources, or track ordinary submissions.

   Two non-mechanical gaps need explicit treatment:

   - `DrawPass::clear_rect` directly calls `cmd_clear_attachments`, while the inspected dynamic command interface has no corresponding attachment-clear method. Use attachment load clears for eligible initial clears and a shared clear pipeline for mid-pass/scissored clears, with color/depth masks and state restoration. Do not make common drawing depend on a raw Vulkan clear callback.
   - Shader creation currently supplies `ShaderInput::SpirV`, and projection conversion assumes Vulkan coordinates. Keep shader artifact selection and clip-space conversion behind backend capabilities/policy. Vulkan may continue to consume glslang-produced SPIR-V. D3D12/Metal shader artifacts and platform bring-up are future work, not something obtained merely by boxing resource handles.

   The checked-in HAL exposes the dynamic ownership and descriptor APIs needed for this direction. Public [DynDevice documentation](https://docs.rs/wgpu-hal/latest/wgpu_hal/trait.DynDevice.html) also describes the interface, but implementation must follow the pinned repository version, not signatures from an unrelated docs.rs version.

   Acceptance: common resource, submission, binding, and draw code has no concrete `hal::vulkan::*`, `ash`, or Vulkan raw-handle dependency. Another backend supplies initialization, capabilities, shaders, and native interop without duplicating the renderer machinery. This does not claim a working D3D12 or Metal backend in this change.

6. **Encode complete WebRender passes with real attachment operations**

   Refactor `RenderPassState`, `RenderDevice`, and `DrawPass` around a pending pass containing its descriptor, attachments, ordered draw/clear commands, and retained resource snapshots.

   `begin_render_pass` establishes that pending pass. Draws snapshot pipelines, descriptors, resource versions, instance ranges, projection values, viewport, and scissor. Mid-pass clears enter the same ordered stream. `end_render_pass(depth_store)` finalizes the operations, prepares resources, and emits one native pass for a normal WR pass.

   Deferring native encoding addresses a concrete API constraint: WR supplies the depth store decision at the end, while HAL needs it when the native pass begins. Starting the native pass immediately and always using STORE would preserve the reviewed defect.

   Before native begin, finish required uploads, flush mapped writes, acquire external images, aggregate compatible resource uses, and emit necessary barriers. Keep uploads and buffer versions immutable for all recorded draws. A deferred draw must not observe a later overwrite of its instance/uniform/table data; use upload slices or copy-on-write versions retained through completion.

   Map operations explicitly:

   | WR operation | Native behavior |
   | --- | --- |
   | `LoadOp::Load` | Load existing valid attachment contents |
   | `LoadOp::Clear(value)` | Initial attachment clear when its scope matches; otherwise preserve untouched contents and encode a scoped clear |
   | `LoadOp::DontCare` | Discard the old contents only over a scope where WR permits that |
   | `end_render_pass(StoreOp::Store)` | Store depth contents |
   | `end_render_pass(StoreOp::Discard)` | Use native depth store-discard and update initialization state |
   | Color output | Store as required by the current WR contract; do not invent an unsupported color discard request |

   The current `RenderPassDescriptor` has color/depth load operations; the end call supplies depth store. Account for that actual contract rather than assuming a color-store field exists.

   Preserve render-area and subrect behavior. Determine whether the pinned HAL can represent the requested area. If it cannot, use conservative full-attachment preservation plus scoped clears; never turn a partial update into a full discard. Keep initialization tracking consistent after discard, partial writes, and failed submission. Empty passes with a clear/store obligation must still perform that obligation.

   Audit copies, readbacks, mipmap generation, resource updates, and external handoffs occurring during a logical pass. Reorder only when dependencies prove it is safe. A genuine interleaving hazard may require an explicitly recorded split that preserves intermediate data and reloads correctly; pipeline, texture binding, and scissor changes alone must never split a pass. Record split reasons so the original per-draw behavior cannot reappear unnoticed.

   Acceptance: a WR pass containing N compatible draws produces one native begin/end pair; initial clears and store-discard are visible in the native descriptor; ordinary mid-pass clears stay in that pass; exceptional splits preserve ordering and are explainable.

7. **Make barriers follow hazards and pass boundaries**

   Replace the unconditional attachment-write rules in both `Texture::transition` and swapchain target transitions. Include depth attachments and mip/view aliases in the same design.

   Plan transitions at pass entry, actual resource role changes, inter-pass hazards, copy/readback boundaries, external acquisition/release, and presentation. Ordinary attachment writes in one native pass use the render pass's ordering; they must not emit COLOR_TARGET-to-COLOR_TARGET or DEPTH_WRITE-to-DEPTH_WRITE barriers between draws.

   Do not replace the current condition with only `old_usage != new_usage`. Separate passes or submissions can still require ordering when usage/layout is unchanged. Track last access and pass/submission identity in addition to usage, use the HAL's ordering capabilities where appropriate, and retain required write-after-write/read dependencies. Read/read reuse should not add a barrier.

   Preserve the transactional `UsageState` behavior: resource state is committed only for successfully submitted work, while aborted recordings release reservations without claiming transitions or initialization happened. External queue ownership and semaphore dependencies remain native integration responsibilities, coordinated with the shared submission lifetime.

   Acceptance: an ordinary multi-draw pass has zero attachment barriers between its draws; render-to-sample, copy-to-render, depth reuse, swapchain presentation, aliasing, and external handoff retain the necessary transitions and synchronization.

8. **Reuse layouts and descriptor groups, including uniform storage**

   Rework `ResolvedProgram::bindings`, `DrawBindings::new`, shader layouts, and the batch path together. A cache alone will not help if every draw creates a distinct projection buffer and therefore a distinct key.

   Intern compatible bind-group layouts, then cache bindings by device generation, layout identity, texture-view identity/subresource range, sampler/filter selection, and buffer identity plus bound range. Use stable resource generations rather than raw pointer addresses. Invalidate on replacement, deletion, imported-image generation changes, and device loss; bound cache growth.

   Put per-draw projection data in an aligned per-frame/submission upload arena. Use dynamic uniform offsets where supported, so many draws bind one arena buffer without allocating one group each. Deduplicate unchanged projection values when worthwhile. Do not overwrite an arena or descriptor group still referenced by pending or submitted draws. Offset alignment, binding range, and maximum uniform-buffer limits must be respected.

   Retain groups/resources through submission completion even after cache eviction. Reuse equivalent resource groups across pipelines sharing a layout; a blend-state change should not force a descriptor allocation solely because the pipeline object differs. Suppress redundant native binds when both the group and dynamic offsets are unchanged.

   Acceptance: N draws with identical bound resources use one cached group in the applicable arena/layout scope, rather than N allocations. Changing only projection values changes uniform data/offsets; changing a texture, sampler, buffer range, or generation produces a correct miss. In-flight resources are never mutated or destroyed early.

9. **Remove the AA workaround and retain a shared correctness investigation**

   Remove `quad_instances.rs`, its shader-name detection and packed bit-field rewriting, `VULKAN_AA_GRID`, additional quad part handling, shader-build defines, and VAO/draw bookkeeping introduced solely for expanded instances. Preserve normal upload and instance-range behavior. Workaround-specific expansion assertions belong in the archived implementation.

   Retain or extract backend-independent visual reproductions from `vulkan_clip_tests.rs`. Reproduce transformed AA joins, fractional coordinates, clipping, and varying scales on the retained rendering paths. Use those cases to establish whether the defect comes from shared geometry, transforms, shader generation, or rasterization-sensitive vertex placement.

   Any necessary correction belongs in shared primitive generation or shared shader geometry, with ordinary data passed to every device backend. Do not move the same shader-name/packed-byte workaround into the new common HAL module and call it fixed. Do not assume the old commit's proposed root cause is proven.

   Acceptance: the device backend contains no quad-specific rewriting; shared visual cases demonstrate the correction or document a remaining blocker. A reproduced visible regression blocks claiming the review response complete even though the old workaround has been archived.

10. **Implementation sequence and validation on vulkan3 only**

    Carry out these work stages in dependency order, then integrate their changes into the existing logical commits and groups according to section 1. The stage order does not prescribe the final commit order or authorize squashing the original groups. Validate each meaningful stage, without requiring archived fragments to build independently.

    | Stage | Change | Evidence required before continuing |
    | --- | --- | --- |
    | A | Dependency cleanup, vendor restoration, and complete deferral/fallback for patch-dependent features | Dependency graph and retained capability paths are coherent; no local vendor patches remain |
    | B | Remove AA backend workaround; preserve portable reproductions | Normal instance semantics restored; geometry regression tracked explicitly |
    | C | Dynamic HAL ownership and native integration boundary | Resource creation/destruction, failure rollback, completion retention, and non-Vulkan-specific interfaces verified |
    | D | Whole-pass collection, load/store mapping, and hazard-aware barriers | Native pass/operation traces and pixel results demonstrate WR pass semantics |
    | E | Binding/layout cache and uniform arena | Allocation counts, invalidation, offsets, and in-flight lifetimes verified |
    | F | Shared geometry correction if required; Gecko/Wrench integration and docs | Retained features work; unsupported/deferred features are accurately described |

    Stages D's pass encoding and barrier changes form one correctness unit: do not remove dependencies while draws still live in separate native passes. Keep changes reviewable within that unit without introducing a known unsafe intermediate implementation.

    Required focused coverage:

    - Adapt existing resource, submission, pass, binding, texture, swapchain, and failure tests to the shared HAL layer. Add command-trace/count assertions for the reviewed performance properties, not just image comparisons that could pass with the old architecture.
    - Cover multi-draw passes, initial and scissored clears, depth store/discard, partial render areas, empty passes, render-to-sample, copies/readback, mipmaps, and aliasing. Include resource mutation between deferred draws to verify snapshots.
    - Cover binding reuse/misses, uniform alignment and arena growth, buffer replacement, view changes, pending submission lifetime, cache eviction, and device loss.
    - Exercise retained external-image ownership and synchronization separately from the deferred WebGPU producer. Check CPU/readback WebGPU canvas presentation, opacity, snapshots, and renderer replacement as applicable.
    - Compile the retained feature configurations and a normal non-Vulkan configuration. Verify wrshell builds with its independent graph. Exercise GTK/X11, Windows Vulkan, and Android lifecycle coverage where the relevant platform infrastructure is available; untested platforms must be reported as such.
    - Run the existing meaningful Vulkan tests with validation enabled, then Wrench image/reftest coverage and targeted Gecko tests. Follow the in-tree Wrench instructions: distrobox/headless for deterministic reference comparisons; a real display for native window/presentation behavior.
    - Compare CPU submission time, native pass count, attachment barrier count, bind-group allocations, upload allocations, and memory growth on `vulkan3` checkpoints. Use existing recorded baseline artifacts if available; do not build or benchmark `vulkan2` or `rejected` for a baseline. Validation-layer runs establish correctness; performance measurements use equivalent non-validation configurations.
    - Capture actual composited output for native-rendering claims. Ordinary browser screenshots can use a software snapshot path; headless Firefox also does not establish hardware Vulkan correctness.

    Redirect slow command output into `artifacts/` logs and inspect those logs separately. Run appropriate formatting/lint/build/test checks only on `vulkan3`, following the relevant repository skills when implementation begins. After satisfactory local validation, ask whether to run `mach try auto`, as required by the repository workflow. Do not submit to Phabricator without explicit approval.

11. **Completion criteria and scope limits**

    Update `gfx/docs/VulkanWebRenderImplementation.md` and `gfx/docs/VulkanWebRenderOverview.md` to describe the shared HAL architecture, the remaining native Vulkan boundary, pass/binding lifetime rules, glslang shader path, and any deferred WebGPU fast path. Update build/configuration help and tests at the same time as removed options.

    The review response is complete only when all of the following hold:

    - `vulkan2` remains at its original commit and untouched by implementation or validation.
    - `vulkan3` preserves the original logical commit split, groups, and relative order except for documented review-required changes; the old-to-new commit map accounts for every original commit and every new prerequisite.
    - `rejected` preserves every removed implementation, with a removal ledger maintained on `vulkan3`; no builds or checks have run on the archive.
    - No added high-level wgpu wrapper, forced wrshell alignment, or branch-local vendor patch remains.
    - Common rendering uses dynamic HAL resources and APIs; raw Vulkan is confined to native integration.
    - Native pass boundaries and attachment operations reflect WR's contract, with no routine per-draw attachment barriers.
    - Bind-group allocation scales with unique resource combinations and arena changes rather than draw count.
    - No backend-specific AA geometry workaround remains; retained rendering has demonstrated correctness.
    - Removed capabilities have a working supported fallback or are explicitly unavailable, with no stale selectors, tests, or documentation suggesting otherwise.

    Upstream acceptance of deferred patches and complete D3D12/Metal backends are separate follow-up work. They must not be used to postpone fixing the existing Vulkan backend's shared architecture, pass structure, or binding costs.

12. **Execution record and structural exceptions**

    The implementation is folded into 107 retained commits in groups 02 through
    26. Shader compilation group 04 comes first, before groups 02 and 03; the
    other groups retain their relative order. Group 01 disappears. No replacement tail of
    corrective implementation commits is added. The final documentation commit
    also carries this plan and ledger.

    - The wrapper/wrshell commits and optional Naga compiler commit are removed
      at their original positions. HAL source replacement and dependency patches
      move to the first device commit in group 02, where they become necessary.
      The standalone lockfile resolves wrshell's independent graph alongside HAL.
    - Dynamic resource ownership is introduced in the original resource commits.
      Native Vulkan extensions stay in the corresponding native integration
      commits. The common clear pipeline replaces native attachment clears in the
      original clear commit. Deferred passes, uniform arenas and binding reuse
      replace the original draw/program/pass implementations at their source.
    - Groups 20 and 21 retain native publication fixtures and consumer
      compatibility work. Their vendor-dependent WebGPU producer commits are
      removed. The CPU external-image integration commit includes WebGPU's
      existing readback fallback.
    - The AA workaround commit becomes the shared geometry correction, with its
      retained renderer reproduction. Its backend-specific packing never enters
      the rewritten stack.
    - Prefix compilation exposed stale VAO and capability APIs in tests from the
      original stack. Those adaptations are folded into each affected test's
      introducing commit. This brings required API repairs forward without
      moving unrelated later behavior into those commits.
    - Binding keys use retained resource identities: each cached value owns the
      resources and layout referenced by its key, preventing address reuse while
      the entry exists. Replacement objects therefore cannot alias live keys.
      Draw binding lookup is bounded and cleared when recording ends; references in
      uniform arenas are weak to avoid retaining completed recordings.

    The surface lifecycle API is named `set_wgpu_surface` throughout Rust and
    `wr_renderer_set_wgpu_surface` at the C ABI, including Gecko and Wrench
    callers. The rename is folded into the original introducing commits.

    The shared external-texture registry accessor is named
    `wgpu_external_textures` in WebRender, its tests, and Gecko/Wrench callers.
    Native Vulkan import and synchronization APIs retain their existing names.

    The test-only offscreen output accessor is named `wgpu_test_output` in the
    shared backend and all renderer test callers.

    Adapter selection follows the user's edited single-loop implementation.
    Device::new is its sole production caller and rejects empty/whitespace-only
    requested names before Vulkan initialization, then lowercases the request
    before selection. The first name match wins; an unmatched request falls back
    to device priority. Equally ranked adapters retain enumeration order. Tests
    cover caller validation, fallback and early matching. The previous selector
    remains archived on rejected without builds or checks on that branch.

    Audit finding F01 is addressed in the existing group 25 readback commit.
    Sampling-only registered textures are sampled into owned scratch storage
    before capture, preserving view swizzles and queued writes. Copy-capable
    textures keep direct readback, including integer formats. The software Vulkan
    regression failed with empty output before the fix and passes with sync
    validation afterward. The native DMA-BUF acquire/capture/release test passes
    on Intel Iris Xe with validation layer 1.4.321. The complete DMA-BUF/timeline
    suite passes 21/21; both formerly failing tests also pass five repeat runs.
    Layer 1.4.313 produced the imported-timeline false positive fixed upstream
    in Vulkan-ValidationLayers issue #10211. Production synchronization and
    vendored code remain unchanged; the local runner now selects the fixed layer.

    Audit finding F02 is addressed by one supports_float_color_format policy,
    shared by trilinear texture allocation, color pipeline creation and blit
    source validation. Hardware format capabilities replace the blitter's
    three-format whitelist. Allocation no longer demands blend support; actual
    blend pipelines still do. Optional float32 filtering is enabled when exposed.
    RGBA8, BGRA8, R8, RG8, R16, RG16 and RGBAF32 pass native mipmap upload and
    precision tests on Intel hardware and software Vulkan without CPU conversion.
    The regression is folded into texture-update integration, where RenderDevice
    can generate mipmaps automatically after upload; earlier commits retain the
    shared policy, feature selection and blitter changes at their own boundaries.

    Audit finding F03 separates immutable BindingResources from inline per-draw
    bindings. Cache hits share one resource owner, retaining layouts directly;
    each draw retains its own pipeline and dynamic projection offset. Resource
    resolution visits only reflected slots. SmallVec keeps resolved resources
    and lookup identities inline for every generated shader (16 textures and
    four storage buffers), while permitting larger future variants to spill.
    Cache hits no longer allocate these lists or an Rc<DrawBindings>, and no
    cached resource vectors are cloned. This does not claim that the entire
    command-recording path is allocation-free or provide a frame-rate estimate.
    Intel validation passes 71 focused tests; software Vulkan passes 45. New
    tests cover shared ownership across pipelines and offsets, filter-key
    separation, lifetime after eviction, generated inline capacity, and visiting
    only consumed slots. The non-capture Rust library also compiles. The fix is
    folded into the original binding, cache, program, slot and draw commits.

    Audit finding F04 makes Submission own the recording's BindingCache, shared
    by ordinary draws, blits and mip generation. Resolving a batch restores the
    cache even on error. Successful submission and abandoned-recording teardown
    clear the lookup state; recorded draw references preserve GPU lifetimes.
    The regression first reproduced twenty 64 KiB uniform reservations for two
    ten-level mip chains. It now observes one 64 KiB arena with all twenty passes
    and exact pixels preserved, survives a rejected binding request, and starts
    a fresh arena on the next recording. Intel validation passes 97 focused
    tests and software Vulkan passes 53; the non-capture library also compiles.
    No full Firefox rebuild is needed. Cache ownership enters with the original
    batch-upload/cache commit; mipmap and bound-draw callers retain their groups.

    Audit finding F05 shares the recording's uniform arena with scoped clears.
    Clear parameters occupy immutable aligned slots in pooled upload storage;
    dynamic offsets select a 32-byte binding range. Clear descriptor groups are
    reused per pipeline and arena buffer, with color/depth masks kept in the
    pipeline key. Clear pipeline variants survive reuse of a submission encoder;
    descriptor lookups and arena state reset when recording ends. The direct
    clear helper and deferred render-pass clears use the same recording cache.
    Initial full-attachment load clears continue to bypass draw-based clearing.
    The regression reproduced 100 descriptor groups for 100 clears. It now uses
    one group and one arena, then checks growth to two arenas/groups while
    preserving overlapping clear order and pixels in one pass. Draws and clears
    share one arena in the existing interleaving test. Pool reuse, abandoned
    recordings, color-only/depth-only masks and native external-texture tests
    pass: 106 focused Intel tests and 65 software Vulkan tests, including the
    corrected test teardown. Non-capture compilation and focused lint pass.
    The shared uniform allocator is folded into its original cache commit;
    pooled clear allocation enters with the original rectangular-clear commit.

    Audit finding F06 replaces boxed retention with an identity-keyed map of
    Rc<dyn Any>. Recording::keep borrows the caller's Rc and clones it only on
    first insertion, so repeated keeps allocate neither boxes nor Rc ownership.
    Identity is the view/resource object's pointer, never the raw image handle.
    Upload-recycling guards use typed vector storage. Draws retain shared binding
    resources and pipelines; clears retain their shared bindings, avoiding
    per-operation snapshot retention. Completion releases resource references
    before recycling uploads. Abandonment also drops unexecuted commit callbacks;
    state commits and external publication callbacks retain their original
    execution semantics and remain separate from resource deduplication.
    The regression first reproduced 100 stored references for 100 keeps of one
    object; it now observes one reference. Tests also cover repeated transitions,
    distinct views of the same image, completion/discard and upload-pool reuse.
    All 116 focused Intel tests, 82 software Vulkan tests and four external-image
    integration tests pass. Debugger-enabled test targets and the non-capture
    library compile; focused lint passes. The new retention API enters with the
    first submission commit, typed upload guards with the upload queue, and
    callers/tests remain in their original introducing commits.

    The NVIDIA mixed-format upload fix is included in the original upload-handle
    commit. Chunk lengths are aligned for the largest supported texel, so a
    following RGBAF32 chunk remains aligned after an R8 or BGRA8 chunk. Its native
    regression passes on Intel hardware and software Vulkan. Pure layout tests
    also cover small driver alignment requirements, and the native test checks
    nonzero floating-point texels. The remaining
    alpha-probe and window-policy work is kept separate until corrected and
    validated.

    Alpha probing and concurrent GTK window-policy edits are
    preserved separately as working-tree changes and excluded from these commits.
    The rewrite publishes Git metadata without checking out or resetting files.
    `vulkan2` remains unchanged. The `rejected` archive retains the original
    history, the pre-selector snapshot and snapshot
    `460f5c9d6dbdf3cb8aa19dbfd604f5f789686023` before F03, plus snapshot
    `fd9a0b68d78a933932b31f37e66170b3bb6917c9` before F04's shared uniform
    arena and `90b4dda647b8bc46ddda1824f1571dee82f2e5bb` before F05's clear
    bindings. Snapshot `44c97b0c3e95a93b35bb8d7604ca3b376102d720` preserves
    the boxed retention implementation before F06. No builds or checks ran on
    either branch.

    Validation on the accepted implementation:

    - One grouped Firefox binaries build, after configuration, vendor, FFI and
      HAL changes were ready together. No full rebuild per commit.
    - Wrshell's independent dependency graph and non-Vulkan WebRender compile.
    - Nine configure tests, 85 focused HAL tests and 11 renderer tests pass.
      Runtime Vulkan tests use llvmpipe with synchronization validation.
    - Twenty-five SWGL clip reftests and 42 Gecko gtests pass.
    - The multi-draw trace test records three draws and a scoped clear in one
      native pass, one draw bind-group allocation and no in-pass attachment
      barriers, including a projection and blend-state change.
    - Standalone Rust compilation, including test targets, is checked at every
      retained Rust-changing prefix. This is not a full Gecko build per prefix.
    - Formatting and focused license/whitespace lint pass. Partial gfx docs
      generation reports existing global/toctree warnings; no Vulkan-page
      warning was identified, and a clean full documentation build is not claimed.

    Hardware is available outside the sandbox; missing /dev/dri inside it was
    an isolation restriction. The initial software X11 run passed 23 of 25 cases;
    those two X11-specific prerequisite cases were not rerun in this follow-up.
    Native DMA-BUF/timeline and owned/view-swizzled readback tests now pass on
    Intel Iris Xe with Mesa 26.2.4 and validation layer 1.4.321. Windows/Android
    runtime coverage, composited hardware output, complete end-to-end WebGPU
    canvas fallback coverage, and comparative CPU timing/memory measurements
    have not been established here. Operation-count assertions are
    evidence of the reviewed structural improvements, not a measured frame-rate
    claim. CI and supported hardware remain follow-up validation work.

13. **Deferred upstream proposals**

    No local vendor edits are retained and no proposal has been submitted.
    The original implementations and reproductions remain available in `rejected`.

    - Naga GLSL/SPIR-V: extract focused shader cases from `b4f1b9b902a` for the
      GLSL declaration/order, built-in and SPIR-V emission changes. Submit each
      demonstrated compiler defect independently with its input and expected
      validated output; remove the optional compiler mode until upstream support
      can be consumed through a normal dependency update.
    - wgpu-core external texture initialization: extract the import/publication
      cases from `8f489c3114b` and the initialization responsibility from
      `5e72aebf05b`. Specify when externally allocated textures become initialized
      and how transitions used for external publication complete initialization.
      Uninitialized imports must never be marked cleared to bypass that contract.
    - Timestamp normalization: `368aff5f18a` is archived with the rejected Naga
      route. Reproduce independently against unmodified upstream Naga before
      proposing a shader change; the retained glslang backend does not justify
      carrying that vendor workaround.

14. **Original-to-rewritten commit map**

    `retained` means the stable patch content is unchanged; `modified` includes
    path/API migration and review-required replacements. `dropped` entries remain
    archived. Group numbers are unchanged. The final documentation entry uses
    `self` because embedding its own Git hash is circular; the external full map
    is `artifacts/vulkan3/final-commit-map.json`.

    | Original | Group | Result | Replacement |
    | --- | --- | --- | --- |
    | `78cb94f83be7` | 01 | dropped | — |
    | `60b776f3a4f7` | 01 | dropped | — |
    | `2161add7bcf7` | 02 | modified | 196be81a0165 |
    | `4adabe095056` | 02 | modified | 4e8c855f6214 |
    | `294cc9a73f38` | 02 | modified | a45ac8e31baa |
    | `09fd042fccb0` | 02 | modified | db35b55febcd |
    | `10470f49d89b` | 02 | modified | 08fecad9c9a6 |
    | `7a90f2b8d84e` | 02 | modified | e9263e7a36e8 |
    | `62cc452e27b8` | 02 | modified | 4f363e4c94f8 |
    | `eeb3596c212c` | 03 | modified | 56e6e95ab8c6 |
    | `3599e2106978` | 03 | modified | cca85f2e2254 |
    | `326760d7a342` | 03 | modified | 94818e631baf |
    | `3d0d0aa8f9bc` | 03 | modified | ca6404342b8d |
    | `84c41622303b` | 03 | modified | 6be504f33f5e |
    | `b3c26b78830e` | 03 | modified | b30df116f239 |
    | `42a6fb4a8812` | 03 | modified | 2a9f3314a2f3 |
    | `b0ae972df3d3` | 04 | modified | 587779128914 |
    | `b0fa715d52f0` | 04 | retained | 2807be1a1010 |
    | `70ce812a4554` | 04 | retained | a3e74f852e5b |
    | `b4f1b9b902a2` | 04 | dropped | — |
    | `ad07b649a26f` | 05 | modified | 7f1c6e2450b0 |
    | `b2cd1e3a23e8` | 05 | modified | f13d82e5d053 |
    | `3bfd37b5762a` | 05 | modified | 1054b6b3acb1 |
    | `b4a44242ac46` | 05 | modified | 9b8e8cd10797 |
    | `980cb73c95f6` | 05 | modified | 9fb9c3515e3c |
    | `e830c3ebf715` | 05 | modified | 1fb33477f95e |
    | `ac45aca097d6` | 06 | modified | 20a7998a640f |
    | `efb8a2906bdf` | 06 | modified | d3642d9531d9 |
    | `19b5509f1aca` | 06 | modified | e40434b004ff |
    | `b8f63874c6c7` | 06 | modified | 150901a3b5d1 |
    | `30632ba4afe2` | 07 | modified | 639733c4597e |
    | `95e51420936e` | 07 | modified | 8cb9d7cd633f |
    | `1aab5be7497c` | 07 | modified | 413395808949 |
    | `0a268aa71dfe` | 07 | modified | 601adb0f7633 |
    | `33b13415667e` | 07 | modified | 86d673d06970 |
    | `f1110468f674` | 08 | modified | 622066fabf64 |
    | `cce19010628b` | 08 | modified | dabeee4b3170 |
    | `8a46ac9495c0` | 08 | modified | eb2b9d33c6bf |
    | `acb71dac88e4` | 08 | modified | 4236301a88a7 |
    | `42f00ba8d752` | 08 | modified | bfdbbe2a3da0 |
    | `631e895d6bca` | 09 | modified | 090eb09233cc |
    | `b83668cc0de9` | 09 | modified | 7780a4b3e2f8 |
    | `393286d0ac97` | 09 | modified | 86f222bbaacc |
    | `e337d7d1e2ce` | 09 | modified | b7e20b8a343e |
    | `358e8491a7b6` | 10 | modified | b450df4c786a |
    | `af1645f6b24f` | 10 | modified | 851579172447 |
    | `559342f8042d` | 10 | retained | cdf73b8cabef |
    | `7840224927d7` | 10 | modified | 75db70180944 |
    | `83475521c4c0` | 10 | modified | c9bf69abc675 |
    | `48954173546d` | 10 | retained | 1af52d830ff9 |
    | `798ce76142f1` | 11 | modified | c9f9c572b1cd |
    | `c8a02cc63a3c` | 11 | modified | 2c035a8a10eb |
    | `af9cbd9661a8` | 11 | modified | 35b5997751f0 |
    | `9897f9a13372` | 11 | modified | e7e5e21555a9 |
    | `5d296edf7639` | 11 | modified | a42c416e4b02 |
    | `1315f89d0654` | 11 | modified | 6ffade6d832a |
    | `e57ec0c8d932` | 11 | modified | 2ad8600df801 |
    | `683186f7c779` | 11 | modified | d3cae48a1aad |
    | `d9db220b5e9f` | 12 | retained | 96148a23dbcd |
    | `2dadaa903fe6` | 12 | modified | 970b6003e42d |
    | `85240e5e1155` | 12 | modified | 0b5a3cd820ec |
    | `8aae32ced9fa` | 13 | modified | 3705246e06bb |
    | `f199df795793` | 13 | modified | 3d608653fa26 |
    | `4d54233533aa` | 13 | modified | e32e37066380 |
    | `0e1e918cbd13` | 13 | modified | a5343c06d862 |
    | `8fcddf9eb83c` | 14 | retained | 3d5a72ee96b1 |
    | `38fb741a2c0f` | 14 | modified | c37841fc7c3d |
    | `cc85ce57ff8d` | 14 | retained | 76e3b1ba46e0 |
    | `b1c555f6d351` | 15 | modified | 1cac4ac45166 |
    | `baa80b092733` | 15 | retained | 9511e51c6cdf |
    | `b06413072919` | 15 | retained | 800a36fd206d |
    | `83eff4cbc29e` | 16 | retained | 5cf0dac9c0c2 |
    | `5ebdf9a65a6b` | 16 | retained | bbd45de0b644 |
    | `0ac220c875bf` | 16 | retained | af41e6644a94 |
    | `6e8044db7211` | 17 | retained | b7b479409e04 |
    | `2c46dc8b67f3` | 18 | modified | 149da749197d |
    | `cb04bc2f6c93` | 18 | modified | 37d3585b3903 |
    | `c317a37fc534` | 18 | modified | 5924b6d50a7f |
    | `8edd2636cb1d` | 18 | modified | 1581c26c73b6 |
    | `fe6e1a470e03` | 18 | modified | 83e9635634b8 |
    | `c68f17cb7af0` | 19 | modified | 66ab99b8e0a8 |
    | `11639bb7abd3` | 19 | modified | 49ed5857f45b |
    | `e72654c9cd1d` | 19 | modified | ecd92aff1d8d |
    | `f677b658d0b5` | 19 | retained | a8b00170672c |
    | `9d7264265ab1` | 19 | retained | df5717e88492 |
    | `abe64aab5996` | 19 | modified | 8d98a340cdb5 |
    | `02dd2bf53a0d` | 19 | retained | 563b16611093 |
    | `c75daf527fcc` | 19 | retained | eff700040f18 |
    | `58a2a5d51777` | 20 | dropped | — |
    | `cff3df7c3412` | 20 | dropped | — |
    | `752fa542f935` | 20 | dropped | — |
    | `8f489c3114b8` | 20 | dropped | — |
    | `e90f920fca16` | 20 | dropped | — |
    | `5e72aebf05b1` | 20 | modified | f075127e27de |
    | `7005ab32b38d` | 20 | modified | e6025e00e586 |
    | `27ae4c629952` | 20 | dropped | — |
    | `368aff5f18a0` | 20 | dropped | — |
    | `0bb5fdff475a` | 21 | dropped | — |
    | `09fd2265f079` | 21 | modified | 2d19af965d03 |
    | `a7dfd7490f54` | 21 | modified | 7f31eb89b7dd |
    | `8bf6e49bd28d` | 21 | modified | 6489b315aae6 |
    | `7555e599aa2a` | 22 | modified | 21aeef80d14e |
    | `21a1597a85d8` | 22 | modified | cacaa49bb8ed |
    | `a76faa4f14c3` | 22 | modified | 1166a4d3c1fd |
    | `f20ef38ed415` | 22 | modified | 96e6820c4504 |
    | `ba1d51bc97e0` | 22 | modified | 38515c2e1d6d |
    | `6c53c1f15cb7` | 23 | modified | 8ec1b1e3adf0 |
    | `907af251d8e9` | 23 | retained | 8e0ac5cb479e |
    | `a9b3f3f112b9` | 23 | modified | a07784030916 |
    | `faa83a202320` | 23 | modified | 5aada20b24c4 |
    | `296f8176badd` | 23 | modified | ac6e2efee65e |
    | `92004128f252` | 23 | retained | cbf5853970f2 |
    | `5b6860919eb9` | 24 | modified | 83b9fecc9326 |
    | `381ad23c8c5a` | 24 | modified | 4f055372a734 |
    | `7a40b8834ec9` | 24 | modified | 7d56eeef6ebc |
    | `730125a913e5` | 25 | modified | 2dee24c6cc13 |
    | `713fdfeb72fa` | 26 | retained | e37610979494 |
    | `638c03f6eb84` | 26 | modified | self (documentation commit containing this plan) |

Shader compilation is mandatory whenever the Vulkan Cargo feature is enabled.
The shader infrastructure and glslang integration precede the device backend;
generated-module integration and GPU shader tests remain with their runtime
prerequisites. The separate shader-availability cfg and runtime fallback are removed.
