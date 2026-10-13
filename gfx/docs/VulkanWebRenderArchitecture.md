# Vulkan WebRender architecture

Vulkan is an optional GPU backend beneath WebRender's shared renderer. It reuses
scene building, batching, render tasks, caches, and composition. Enabling it
requires both build support and a startup preference.

The main responsibilities are divided as follows:

| Layer | Responsibility |
| --- | --- |
| Firefox compositor integration | Native windows, visibility, frame completion, and recovery |
| Shared WebRender renderer | Scenes, resource updates, render tasks, and draw state |
| HAL rendering backend | GPU resources, pipelines, render passes, and submissions |
| Native Vulkan integration | Device creation, shader input, and external image sharing |
| Dynamic `wgpu-hal` interfaces | GPU resource, command, and presentation operations |

The backend follows these architectural decisions:

- WebRender owns its GPU device independently of WebGPU.
- Existing GLSL shaders are compiled into embedded SPIR-V using glslang at build time.
- Shader tools run on the build host and are not required by the installed browser.
- Upstream wgpu and Naga dependencies remain unmodified.
- GPU commands are batched with explicit synchronization and resource retention until completion.
- Upload buffers and temporary textures are pooled; pipelines and resource bindings are cached.
- Draw instances use pooled uploads for each draw.
- Texture blits and mipmap generation run on the GPU using supported native formats.
- Final composition renders directly into swapchain images without a routine presentation copy.
- Window surfaces can be detached and replaced while retaining the renderer's device and resources.
- Hidden X11 windows skip presentation while resource updates continue.
- Permanent GPU failures enter Firefox's existing renderer recovery path.
- Diagnostics report the backend and GPU actually running.

External images and capture use these paths:

- Native image sharing uses explicit ownership transfers and GPU synchronization.
- Linux GL producers can share compatible RGB images through DMA-BUF.
- CPU images are uploaded through the shared buffer path.
- WebGPU canvas presentation uses CPU readback and upload; native WebGPU sharing is deferred.
- Temporarily unavailable external images defer rendering and trigger retries.
- Synchronous screenshots use an additional GPU copy only when requested and supported.

Platform support has these boundaries:

- Vulkan startup selection supports Linux/X11 and Windows; Wayland and macOS are unsupported.
- Android retains accelerated OpenGL until native Vulkan video-image transport is available.
- Rendering requires Vulkan 1.1 and suitable device and surface capabilities.
- Native image sharing requires additional capabilities and is limited to Linux RGB images.
- GL image sharing additionally requires linear storage.
- X11 support is probed before activation; unsupported window transparency can require software rendering.
- Transparent Windows windows are unsupported.
- Native hardware-video transport is unavailable; GTK FFmpeg decoding falls back to software.
- Native validation has focused on Linux/X11; Windows and Android still require native validation.

The current integration also has these limitations:

- Presentation uses full-frame composition; native layer composition and incremental presentation are unavailable.
- Asynchronous screenshots and composition recording are unavailable.
- Runtime shader overrides and debug overlays are unsupported.
- Non-instanced drawing, multisampling, and shader storage-buffer tables are unsupported.
- Wrench supports scene display, initialization, and invalidation tests; broader harness support is incomplete.

See the [overview](VulkanWebRenderOverview.md) for setup and the
[implementation guide](VulkanWebRenderImplementation.md) for detailed contracts.
