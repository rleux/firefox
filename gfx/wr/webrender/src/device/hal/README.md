# HAL bootstrap

This experimental module uses `wgpu-hal` 30.0.0 directly. It initializes a device,
clears offscreen RGBA8 and D32 targets, and synchronously reads their contents.
It does not yet execute WebRender display lists or replace the GL `Device` used
by `Renderer`. The existing GL constructor, caches, and default features retain
their existing behavior.

`hal` enables the generic resource/command implementation; `hal-vulkan` also
enables Vulkan construction and its native-image probe. HAL types stay within
this module and its diagnostic entry point. No Vulkan handles enter scene or
frame construction. Metal and D3D12 constructors are future work; only Linux
Vulkan has been built and executed.

## Wrench

From `gfx/wr/wrench`, in the prescribed Linux reference container with a Vulkan
loader, ICD, and validation layer installed:

```sh
WRENCH_VULKAN_ICD=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  python3 script/headless.py --backend hal --hal-backend vulkan --hal-validation test_hal
```

ICD filenames depend on the distribution. `WRENCH_VULKAN_ICD` sets
`VK_DRIVER_FILES`; without it, existing loader environment/configuration applies.
The launcher builds `hal-vulkan`, skips OSMesa setup, and forwards the arguments.
`--hal-adapter NAME` requires exactly one case-insensitive substring match. Without
a filter, selection prefers discrete, integrated, virtual, then CPU adapters.
Failure to open the selected adapter is an error. Backend and adapter identity,
device type, driver, requested validation, and explicit ICD selection are logged.
Requested validation requires `VK_LAYER_KHRONOS_validation` to be discoverable.

`test_init` performs a color/depth clear and readback; `test_hal` adds differently
sized targets, invalid-size rejection, and native-image tests. `--size WIDTHxHEIGHT`
sets the initial target. HAL selection requires `--headless` and rejects GL-only
construction options. Reftest, rawtest, shader, invalidation, capture, and windowed
HAL rendering are not implemented yet and fail explicitly. No GL fallback occurs.

GL remains the default:

```sh
python3 script/headless.py reftest
```

A build with both `headless,hal-vulkan` can exercise both bootstrap selectors.
The HAL command needs no GL context even when GL support is compiled in.
`python3 script/test_headless.py` checks launcher routing without a GPU.

## Ownership and target contract

The bootstrap owns its device, queue, and instance. Resources borrow that device;
command ownership ends only after GPU completion, before resources are destroyed.
One synchronous submission/fence is used per readback. CPU frame numbers and GL
resource IDs are not completion tokens or HAL handles.

Targets have positive, device-limited dimensions, one mip and sample, and no
window-system surface. Transitions explicitly cover attachment writes, transfer
reads, and host reads. Readback removes aligned row padding and returns packed
RGBA8 and native `f32` depth rows from texture coordinate y=0 onward. No GL-style
vertical flip is applied. Native-image tests vary both x and y to check this
boundary. Shader projection and final framebuffer coordinates remain Stage 4 work.

The Vulkan probe allocates an image and upload memory through a raw producer path
on the same logical device and queue family. The producer uploads patterned pixels,
transitions to transfer-source layout, and signals an acquire semaphore. HAL wraps
the borrowed image with external memory ownership, waits for acquisition, reads it,
and signals release. The producer waits for release; destroying the HAL wrapper
invokes its callback without destroying the producer's allocation. Error cleanup
removes unsubmitted semaphore hooks and waits before freeing resources.

This proves same-device image wrapping and GPU semaphore handoff. It does not
prove DMA-BUF, cross-process/device import, queue-family transfer, video planes,
or native compositor integration. The later external-image/target interfaces must
represent device identity, format/planes, extent, subresources, valid/dirty region,
ownership, initial/final usage, and acquire/release synchronization. Platform
adapters own OS handles and handle conversion. The existing GL external-image
contract remains unchanged; this probe is not an implementation of that contract.

## Integration limits

Standalone `gfx/wr/Cargo.lock` pins the released HAL 30.0.0 dependency graph while
retaining the pre-existing HAL 27 GUI dependencies. Firefox's root workspace
patches HAL 30.0.0 to a Git revision; its lockfile records the new optional edges,
but full Gecko resolution/build and API compatibility with that patched source
require the full checkout and are unverified here. Non-Linux builds are unverified.

Stage 4 must integrate actual WR shaders, resources, and draws through the selected
device seam. Passing `test_hal` is a bootstrap gate, not a Vulkan Wrench reftest pass.
