//! Vulkan Surface and Swapchain implementation using native Vulkan surfaces.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::any::Any;

use ash::{google, khr, vk};
use wgpu_sync::{Mutex, MutexGuard};

use crate::vulkan::{
    conv, map_host_device_oom_and_lost_err,
    semaphore_list::SemaphoreType,
    swapchain::{
        Surface, SurfaceTextureMetadata, Swapchain, SwapchainSubmissionSemaphoreGuard, WindowHandle,
    },
    DeviceShared, InstanceShared, PnextChain,
};

#[path = "generation.rs"]
mod generation;
pub(crate) use generation::Generation;

pub(crate) struct NativeSurfaceOwner {
    raw: vk::SurfaceKHR,
    functor: khr::surface::Instance,
    instance: Arc<InstanceShared>,
}

pub(crate) struct NativeSurface {
    owner: Arc<NativeSurfaceOwner>,
    /// Built from the window's `HWND` (Windows only) to answer the display-HDR
    /// query; `None` for non-Win32 surfaces.
    #[cfg(windows)]
    hdr_source: Option<crate::auxil::dxgi::hdr::DxgiHdrSource>,
    /// A caller-provided `pNext` chain to attach to the [`vk::SwapchainCreateInfoKHR`]
    /// of the next swapchain created for this surface.
    ///
    /// Set only through
    /// [`Surface::set_next_swapchain_create_chain()`](crate::vulkan::Surface::set_next_swapchain_create_chain).
    next_swapchain_create_chain: Mutex<Option<PnextChain>>,
}

impl core::ops::Deref for NativeSurface {
    type Target = NativeSurfaceOwner;
    fn deref(&self) -> &Self::Target {
        &self.owner
    }
}

impl NativeSurface {
    pub fn from_vk_surface_khr(
        instance: &crate::vulkan::Instance,
        raw: vk::SurfaceKHR,
        hwnd: Option<WindowHandle>,
    ) -> Self {
        #[cfg(not(windows))]
        let _ = hwnd;
        let functor = khr::surface::Instance::new(&instance.shared.entry, &instance.shared.raw);
        Self {
            owner: Arc::new(NativeSurfaceOwner {
                raw,
                functor,
                instance: Arc::clone(&instance.shared),
            }),
            #[cfg(windows)]
            hdr_source: hwnd.map(|wh| crate::auxil::dxgi::hdr::DxgiHdrSource::new(wh.0)),
            next_swapchain_create_chain: Mutex::new(None),
        }
    }

    pub(crate) fn supports_shared_device(&self, device: &crate::vulkan::Device) -> bool {
        device.shared.queue_index == 0
            && Arc::ptr_eq(&self.instance, &device.shared.instance)
            && device
                .shared
                .shared_queue_group
                .as_ref()
                .is_some_and(|group| group.presentation_fences_enabled)
    }

    #[cfg(test)]
    pub(crate) fn owner_weak(&self) -> alloc::sync::Weak<NativeSurfaceOwner> {
        Arc::downgrade(&self.owner)
    }

    pub fn as_raw(&self) -> vk::SurfaceKHR {
        self.raw
    }

    /// # Safety
    ///
    /// See [`Surface::set_next_swapchain_create_chain()`](crate::vulkan::Surface::set_next_swapchain_create_chain).
    pub unsafe fn set_next_swapchain_create_chain(&self, chain: *mut core::ffi::c_void) {
        *self.next_swapchain_create_chain.lock() = Some(PnextChain::new(chain));
    }
}

impl Drop for NativeSurfaceOwner {
    fn drop(&mut self) {
        unsafe {
            self.functor.destroy_surface(self.raw, None);
        }
    }
}

impl Surface for NativeSurface {
    fn surface_capabilities(
        &self,
        adapter: &crate::vulkan::Adapter,
    ) -> Option<crate::SurfaceCapabilities> {
        if !adapter.private_caps.can_present {
            return None;
        }
        let queue_family_index = 0; //TODO
        {
            profiling::scope!("vkGetPhysicalDeviceSurfaceSupportKHR");
            match unsafe {
                self.functor.get_physical_device_surface_support(
                    adapter.raw,
                    queue_family_index,
                    self.raw,
                )
            } {
                Ok(true) => (),
                Ok(false) => return None,
                Err(e) => {
                    log::error!("get_physical_device_surface_support: {e}");
                    return None;
                }
            }
        }

        let caps = {
            profiling::scope!("vkGetPhysicalDeviceSurfaceCapabilitiesKHR");
            match unsafe {
                self.functor
                    .get_physical_device_surface_capabilities(adapter.raw, self.raw)
            } {
                Ok(caps) => caps,
                Err(e) => {
                    log::error!("get_physical_device_surface_capabilities: {e}");
                    return None;
                }
            }
        };

        // If image count is 0, the support number of images is unlimited.
        let max_image_count = if caps.max_image_count == 0 {
            !0
        } else {
            caps.max_image_count
        };

        // `0xFFFFFFFF` indicates that the extent depends on the created swapchain.
        let current_extent = if caps.current_extent.width != !0 && caps.current_extent.height != !0
        {
            Some(wgt::Extent3d {
                width: caps.current_extent.width,
                height: caps.current_extent.height,
                depth_or_array_layers: 1,
            })
        } else {
            None
        };

        let raw_present_modes = {
            profiling::scope!("vkGetPhysicalDeviceSurfacePresentModesKHR");
            match unsafe {
                self.functor
                    .get_physical_device_surface_present_modes(adapter.raw, self.raw)
            } {
                Ok(present_modes) => present_modes,
                Err(e) => {
                    log::error!("get_physical_device_surface_present_modes: {e}");
                    // Per definition of `SurfaceCapabilities`, there must be at least one present mode.
                    return None;
                }
            }
        };

        let raw_surface_formats = {
            profiling::scope!("vkGetPhysicalDeviceSurfaceFormatsKHR");
            match unsafe {
                self.functor
                    .get_physical_device_surface_formats(adapter.raw, self.raw)
            } {
                Ok(formats) => formats,
                Err(e) => {
                    log::error!("get_physical_device_surface_formats: {e}");
                    // Per definition of `SurfaceCapabilities`, there must be at least one present format.
                    return None;
                }
            }
        };

        // Group the driver's (format, color space) pairs into one entry per
        // format, preserving the driver's format order.
        let mut formats: Vec<wgt::SurfaceFormatCapabilities> = Vec::new();
        for (format, color_space) in raw_surface_formats
            .into_iter()
            .filter_map(conv::map_vk_surface_formats)
        {
            let color_spaces = color_space.to_color_spaces().unwrap();
            match formats.iter_mut().find(|fc| fc.format == format) {
                Some(fc) => fc.color_spaces |= color_spaces,
                None => formats.push(wgt::SurfaceFormatCapabilities {
                    format,
                    color_spaces,
                }),
            }
        }
        Some(crate::SurfaceCapabilities {
            formats,
            // TODO: Right now we're always truncating the swap chain
            // (presumably - we're actually setting the min image count which isn't necessarily the swap chain size)
            // Instead, we should use extensions when available to wait in present.
            // See https://github.com/gfx-rs/wgpu/issues/2869
            maximum_frame_latency: (caps.min_image_count - 1)..=(max_image_count - 1), // Note this can't underflow since both `min_image_count` is at least one and we already patched `max_image_count`.
            current_extent,
            usage: conv::map_vk_image_usage(caps.supported_usage_flags),
            present_modes: raw_present_modes
                .into_iter()
                .flat_map(conv::map_vk_present_mode)
                .collect(),
            composite_alpha_modes: conv::map_vk_composite_alpha(caps.supported_composite_alpha),
        })
    }

    unsafe fn create_swapchain(
        &self,
        device: &crate::vulkan::Device,
        config: &crate::SurfaceConfiguration,
        provided_old_swapchain: Option<Box<dyn Swapchain>>,
    ) -> Result<Box<dyn Swapchain>, crate::SurfaceError> {
        profiling::scope!("Device::create_swapchain");
        let functor = khr::swapchain::Device::new(&self.instance.raw, &device.shared.raw);

        let old_swapchain = provided_old_swapchain
            .as_ref()
            .map(|osc| osc.as_any().downcast_ref::<NativeSwapchain>().unwrap().raw)
            .unwrap_or(vk::SwapchainKHR::null());

        let color_space = conv::map_surface_color_space(config.color_space);

        let original_format = device.shared.private_caps.map_texture_format(config.format);
        let mut raw_flags = vk::SwapchainCreateFlagsKHR::empty();
        let mut raw_view_formats: Vec<vk::Format> = vec![];
        if !config.view_formats.is_empty() {
            raw_flags |= vk::SwapchainCreateFlagsKHR::MUTABLE_FORMAT;
            raw_view_formats = config
                .view_formats
                .iter()
                .map(|f| device.shared.private_caps.map_texture_format(*f))
                .collect();
            raw_view_formats.push(original_format);
        }

        let mut info = vk::SwapchainCreateInfoKHR::default()
            .flags(raw_flags)
            .surface(self.raw)
            .min_image_count(config.maximum_frame_latency + 1) // TODO: https://github.com/gfx-rs/wgpu/issues/2869
            .image_format(original_format)
            .image_color_space(color_space)
            .image_extent(vk::Extent2D {
                width: config.extent.width,
                height: config.extent.height,
            })
            .image_array_layers(config.extent.depth_or_array_layers)
            .image_usage(conv::map_texture_usage(config.usage))
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
            .composite_alpha(conv::map_composite_alpha_mode(config.composite_alpha_mode))
            .present_mode(conv::map_present_mode(config.present_mode))
            .clipped(true)
            .old_swapchain(old_swapchain);

        let mut format_list_info = vk::ImageFormatListCreateInfo::default();
        if !raw_view_formats.is_empty() {
            format_list_info = format_list_info.view_formats(&raw_view_formats);
            info = info.push_next(&mut format_list_info);
        }

        let create_chain = self.next_swapchain_create_chain.lock().take();
        if let Some(chain) = create_chain {
            // SAFETY: The contract on `Surface::set_next_swapchain_create_chain()` keeps
            // the chain valid and unaliased until this swapchain creation returns.
            info.p_next = unsafe { chain.splice_into(info.p_next) };
        }

        let result = {
            profiling::scope!("vkCreateSwapchainKHR");
            unsafe { functor.create_swapchain(&info, None) }
        };

        let raw = match result {
            Ok(swapchain) => swapchain,
            Err(error) => {
                return Err(match error {
                    vk::Result::ERROR_SURFACE_LOST_KHR
                    | vk::Result::ERROR_INITIALIZATION_FAILED => crate::SurfaceError::Lost,
                    vk::Result::ERROR_NATIVE_WINDOW_IN_USE_KHR => {
                        crate::SurfaceError::Other("Native window is in use")
                    }
                    // We don't use VK_EXT_image_compression_control
                    // VK_ERROR_COMPRESSION_EXHAUSTED_EXT
                    other => map_host_device_oom_and_lost_err(other).into(),
                });
            }
        };

        let shared = device.shared.shared_queue_group.is_some();
        let images = match unsafe { functor.get_swapchain_images(raw) } {
            Ok(images) => images,
            Err(error) => {
                if shared {
                    unsafe { functor.destroy_swapchain(raw, None) };
                }
                return Err(crate::vulkan::map_host_device_oom_err(error).into());
            }
        };

        // This fence is only used to throttle acquisition on Windows. It is very important to
        // avoid bad frame pacing when the Vulkan driver is using a DXGI swapchain. See
        // https://github.com/gfx-rs/wgpu/issues/8310 and
        // https://github.com/gfx-rs/wgpu/issues/8354 for more details.
        let fence = if cfg!(target_os = "windows") && !shared {
            let raw = unsafe {
                device
                    .shared
                    .raw
                    .create_fence(&vk::FenceCreateInfo::default(), None)
                    .map_err(crate::vulkan::map_host_device_oom_err)?
            };
            Some(raw)
        } else {
            None
        };

        // NOTE: It's important that we define the same number of acquire/present semaphores
        // as we will need to index into them with the image index.
        let generation = if shared {
            Some(Generation::new(
                raw,
                functor.clone(),
                device.shared.clone(),
                self.owner.clone(),
                images.len(),
            )?)
        } else {
            None
        };
        let (acquire_semaphores, present_semaphores) = if let Some(generation) = &generation {
            (
                generation.acquire_semaphores(),
                generation.present_semaphores(),
            )
        } else {
            let acquire = (0..images.len())
                .map(|i| {
                    SwapchainAcquireSemaphore::new(&device.shared, i)
                        .map(Mutex::new)
                        .map(Arc::new)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let present = (0..images.len())
                .map(|i| Arc::new(Mutex::new(SwapchainPresentSemaphores::new(i))))
                .collect();
            (acquire, present)
        };

        Ok(Box::new(NativeSwapchain {
            raw,
            functor,
            device: Arc::clone(&device.shared),
            images,
            fence,
            config: config.clone(),
            acquire_semaphores,
            next_acquire_index: 0,
            present_semaphores,
            next_present_time: None,
            next_present_chain: None,
            generation,
        }))
    }

    #[cfg(windows)]
    fn display_hdr_info(&self) -> Option<wgt::DisplayHdrInfo> {
        self.hdr_source.as_ref()?.display_hdr_info()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub(crate) struct NativeSwapchain {
    raw: vk::SwapchainKHR,
    functor: khr::swapchain::Device,
    device: Arc<DeviceShared>,
    images: Vec<vk::Image>,
    /// Fence used to wait on the acquired image.
    fence: Option<vk::Fence>,
    config: crate::SurfaceConfiguration,

    /// Semaphores used between image acquisition and the first submission
    /// that uses that image. This is indexed using [`next_acquire_index`].
    ///
    /// Because we need to provide this to [`vkAcquireNextImageKHR`], we haven't
    /// received the swapchain image index for the frame yet, so we cannot use
    /// that to index it.
    ///
    /// Before we pass this to [`vkAcquireNextImageKHR`], we ensure that we wait on
    /// the submission indicated by [`previously_used_submission_index`]. This ensures
    /// the semaphore is no longer in use before we use it.
    ///
    /// [`next_acquire_index`]: NativeSwapchain::next_acquire_index
    /// [`vkAcquireNextImageKHR`]: https://registry.khronos.org/vulkan/specs/1.3-extensions/html/vkspec.html#vkAcquireNextImageKHR
    /// [`previously_used_submission_index`]: SwapchainAcquireSemaphore::previously_used_submission_index
    acquire_semaphores: Vec<Arc<Mutex<SwapchainAcquireSemaphore>>>,
    /// The index of the next acquire semaphore to use.
    ///
    /// This is incremented each time we acquire a new image, and wraps around
    /// to 0 when it reaches the end of [`acquire_semaphores`].
    ///
    /// [`acquire_semaphores`]: NativeSwapchain::acquire_semaphores
    next_acquire_index: usize,

    /// Semaphore sets used between all submissions that write to an image and
    /// the presentation of that image.
    ///
    /// This is indexed by the swapchain image index returned by
    /// [`vkAcquireNextImageKHR`].
    ///
    /// We know it is safe to use these semaphores because use them
    /// _after_ the acquire semaphore. Because the acquire semaphore
    /// has been signaled, the previous presentation using that image
    /// is known-finished, so this semaphore is no longer in use.
    ///
    /// [`vkAcquireNextImageKHR`]: https://registry.khronos.org/vulkan/specs/1.3-extensions/html/vkspec.html#vkAcquireNextImageKHR
    present_semaphores: Vec<Arc<Mutex<SwapchainPresentSemaphores>>>,

    /// The present timing information which will be set in the next call to [`present()`](crate::Queue::present()).
    ///
    /// # Safety
    ///
    /// This must only be set if the VK_GOOGLE_display_timing extension is enabled.
    next_present_time: Option<vk::PresentTimeGOOGLE>,

    /// A caller-provided `pNext` chain to attach to the [`vk::PresentInfoKHR`] of the next
    /// call to [`present()`](crate::Queue::present()).
    ///
    /// Set only through
    /// [`Surface::set_next_present_chain()`](crate::vulkan::Surface::set_next_present_chain).
    next_present_chain: Option<PnextChain>,
    // Shared semaphore vectors must drop before their generation owner.
    generation: Option<Arc<Generation>>,
}

impl Drop for NativeSwapchain {
    fn drop(&mut self) {
        if self.generation.is_none() {
            unsafe { self.functor.destroy_swapchain(self.raw, None) };
        }
    }
}

/// Destroy the swapchain semaphores that are no longer referenced by a live
/// surface texture. A still-referenced semaphore is skipped, as the texture
/// may still submit work using it.
///
/// The vectors are drained.
fn destroy_swapchain_semaphores(
    acquire_semaphores: &mut Vec<Arc<Mutex<SwapchainAcquireSemaphore>>>,
    present_semaphores: &mut Vec<Arc<Mutex<SwapchainPresentSemaphores>>>,
    device: &crate::vulkan::Device,
) {
    for semaphore in acquire_semaphores.drain(..) {
        if let Some(mutex_removed) = Arc::into_inner(semaphore) {
            let semaphore_removed = mutex_removed.into_inner();
            unsafe { semaphore_removed.destroy(&device.shared.raw) };
        }
    }

    for semaphore in present_semaphores.drain(..) {
        if let Some(mutex_removed) = Arc::into_inner(semaphore) {
            let semaphore_removed = mutex_removed.into_inner();
            unsafe { semaphore_removed.destroy(&device.shared.raw) };
        }
    }
}

impl Swapchain for NativeSwapchain {
    unsafe fn release_resources(
        &mut self,
        device: &crate::vulkan::Device,
    ) -> Result<(), crate::DeviceError> {
        if let Some(generation) = &self.generation {
            return generation.retire();
        }
        profiling::scope!("Swapchain::release_resources");
        {
            profiling::scope!("vkDeviceWaitIdle");
            // We need to also wait until all presentation work is done. Because there is no way to portably wait until
            // the presentation work is done, we are forced to wait until the device is idle.
            let _ = unsafe {
                device
                    .shared
                    .raw
                    .device_wait_idle()
                    .map_err(map_host_device_oom_and_lost_err)
            };
        };

        if let Some(fence) = self.fence {
            unsafe { device.shared.raw.destroy_fence(fence, None) }
        }

        destroy_swapchain_semaphores(
            &mut self.acquire_semaphores,
            &mut self.present_semaphores,
            device,
        );
        Ok(())
    }

    unsafe fn acquire(
        &mut self,
        timeout: Option<core::time::Duration>,
        fence: &crate::vulkan::Fence,
    ) -> Result<crate::AcquiredSurfaceTexture<crate::api::Vulkan>, crate::SurfaceError> {
        if let Some(generation) = &self.generation {
            generation.bind_public_fence(fence)?;
        }
        let mut timeout_ns = match timeout {
            Some(duration) => duration.as_nanos() as u64,
            None => u64::MAX,
        };

        // AcquireNextImageKHR on Android (prior to Android 11) doesn't support timeouts
        // and will also log verbose warnings if tying to use a timeout.
        //
        // Android 10 implementation for reference:
        // https://android.googlesource.com/platform/frameworks/native/+/refs/tags/android-mainline-10.0.0_r13/vulkan/libvulkan/swapchain.cpp#1426
        // Android 11 implementation for reference:
        // https://android.googlesource.com/platform/frameworks/native/+/refs/tags/android-mainline-11.0.0_r45/vulkan/libvulkan/swapchain.cpp#1438
        //
        // Android 11 corresponds to an SDK_INT/ro.build.version.sdk of 30
        if cfg!(target_os = "android") && self.device.instance.android_sdk_version < 30 {
            timeout_ns = u64::MAX;
        }

        let acquire_semaphore_arc = self.get_acquire_semaphore();
        // Nothing should be using this, so we don't block, but panic if we fail to lock.
        let acquire_semaphore_guard = acquire_semaphore_arc
            .try_lock()
            .expect("Failed to lock a SwapchainSemaphores.");

        // Wait for all commands writing to the previously acquired image to
        // complete.
        //
        // Almost all the steps in the usual acquire-draw-present flow are
        // asynchronous: they get something started on the presentation engine
        // or the GPU, but on the CPU, control returns immediately. Without some
        // sort of intervention, the CPU could crank out frames much faster than
        // the presentation engine can display them.
        //
        // This is the intervention: if any submissions drew on this image, and
        // thus waited for `locked_swapchain_semaphores.acquire`, wait for all
        // of them to finish, thus ensuring that it's okay to pass `acquire` to
        // `vkAcquireNextImageKHR` again.
        let completed = self.device.wait_for_fence(
            fence,
            acquire_semaphore_guard.previously_used_submission_index,
            timeout_ns,
        )?;
        if !completed {
            return Err(crate::SurfaceError::Timeout);
        }

        let acquire_slot = self.next_acquire_index;
        let acquire_fence = if let Some(generation) = &self.generation {
            generation.prepare_acquire(acquire_slot)?
        } else {
            self.fence.unwrap_or_else(vk::Fence::null)
        };

        // will block if no image is available
        let (index, suboptimal) = match unsafe {
            profiling::scope!("vkAcquireNextImageKHR");
            self.functor.acquire_next_image(
                self.raw,
                timeout_ns,
                acquire_semaphore_guard.acquire,
                acquire_fence,
            )
        } {
            // We treat `VK_SUBOPTIMAL_KHR` as `VK_SUCCESS` on Android.
            // See the comment in `Queue::present`.
            #[cfg(target_os = "android")]
            Ok((index, _)) => (index, false),
            #[cfg(not(target_os = "android"))]
            Ok(pair) => pair,
            Err(error) => {
                return match error {
                    vk::Result::TIMEOUT => Err(crate::SurfaceError::Timeout),
                    vk::Result::NOT_READY | vk::Result::ERROR_OUT_OF_DATE_KHR => {
                        Err(crate::SurfaceError::Outdated)
                    }
                    vk::Result::ERROR_SURFACE_LOST_KHR => Err(crate::SurfaceError::Lost),
                    // We don't use VK_EXT_full_screen_exclusive
                    // VK_ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT
                    other => Err(map_host_device_oom_and_lost_err(other).into()),
                };
            }
        };

        let shared_identity = if let Some(generation) = &self.generation {
            generation.acquired(acquire_slot);
            let identity = self.device.texture_identity_factory.next();
            generation.begin_image(index, identity)?;
            if cfg!(target_os = "windows") {
                generation.wait_acquire(acquire_slot, timeout_ns)?;
            }
            Some(identity)
        } else {
            None
        };

        if let Some(fence) = self.fence {
            unsafe {
                // The `wait_all` argument must be `true` to avoid crash on some Android devices. See https://github.com/gfx-rs/wgpu/pull/8769
                self.device
                    .raw
                    .wait_for_fences(&[fence], true, timeout_ns)
                    .map_err(map_host_device_oom_and_lost_err)?;

                self.device
                    .raw
                    .reset_fences(&[fence])
                    .map_err(map_host_device_oom_and_lost_err)?;
            }
        }

        drop(acquire_semaphore_guard);
        // We only advance the surface semaphores if we successfully acquired an image, otherwise
        // we should try to re-acquire using the same semaphores.
        self.advance_acquire_semaphore();

        let present_semaphore_arc = self.get_present_semaphores(index);

        // special case for Intel Vulkan returning bizarre values (ugh)
        if self.device.vendor_id == crate::auxil::db::intel::VENDOR && index > 0x100 {
            if let Some(generation) = &self.generation {
                generation.close();
            }
            return Err(crate::SurfaceError::Outdated);
        }

        let identity =
            shared_identity.unwrap_or_else(|| self.device.texture_identity_factory.next());

        let texture = crate::vulkan::SurfaceTexture {
            index,
            texture: crate::vulkan::Texture {
                raw: self.images[index as usize],
                drop_guard: None,
                memory: crate::vulkan::TextureMemory::External,
                format: self.config.format,
                copy_size: crate::CopyExtent {
                    width: self.config.extent.width,
                    height: self.config.extent.height,
                    depth: 1,
                },
                identity,
                surface_generation: self.generation.clone(),
            },
            metadata: Box::new(NativeSurfaceTextureMetadata {
                acquire_semaphores: acquire_semaphore_arc,
                present_semaphores: present_semaphore_arc,
                generation: self.generation.clone(),
            }),
        };
        Ok(crate::AcquiredSurfaceTexture {
            texture,
            suboptimal,
        })
    }

    unsafe fn discard_texture(
        &mut self,
        texture: crate::vulkan::SurfaceTexture,
    ) -> Result<(), crate::SurfaceError> {
        if let Some(generation) = &self.generation {
            let metadata = texture
                .metadata
                .as_any()
                .downcast_ref::<NativeSurfaceTextureMetadata>();
            if !metadata
                .and_then(|metadata| metadata.generation.as_ref())
                .is_some_and(|owner| Arc::ptr_eq(owner, generation))
            {
                return Err(crate::SurfaceError::Outdated);
            }
            generation.close();
        }
        Ok(())
    }

    unsafe fn present(
        &mut self,
        queue: &crate::vulkan::Queue,
        texture: crate::vulkan::SurfaceTexture,
    ) -> Result<(), crate::SurfaceError> {
        if self.generation.is_some() {
            return unsafe { self.present_shared(queue, texture) };
        }
        let metadata = texture
            .metadata
            .as_any()
            .downcast_ref::<NativeSurfaceTextureMetadata>()
            .unwrap();
        let mut acquire_semaphore = metadata.acquire_semaphores.lock();
        let mut present_semaphores = metadata.present_semaphores.lock();

        let wait_semaphores = present_semaphores.get_present_wait_semaphores();

        // Reset the acquire and present semaphores internal state
        // to be ready for the next frame.
        //
        // We do this before the actual call to present to ensure that
        // even if this method errors and early outs, we have reset
        // the state for next frame.
        acquire_semaphore.end_semaphore_usage();
        present_semaphores.end_semaphore_usage();

        drop(acquire_semaphore);

        let swapchains = [self.raw];
        let image_indices = [texture.index];
        let vk_info = vk::PresentInfoKHR::default()
            .swapchains(&swapchains)
            .image_indices(&image_indices)
            .wait_semaphores(&wait_semaphores);

        let mut display_timing;
        let present_times;
        let mut vk_info = if let Some(present_time) = self.next_present_time.take() {
            debug_assert!(
                self.device
                    .enabled_extensions
                    .contains(&google::display_timing::NAME),
                "`next_present_time` should only be set if VK_GOOGLE_display_timing is enabled"
            );
            present_times = [present_time];
            display_timing = vk::PresentTimesInfoGOOGLE::default().times(&present_times);
            // SAFETY: We know that VK_GOOGLE_display_timing is present because of the safety contract on `next_present_time`.
            vk_info.push_next(&mut display_timing)
        } else {
            vk_info
        };

        if let Some(chain) = self.next_present_chain.take() {
            // SAFETY: The contract on `Surface::set_next_present_chain()` keeps the chain
            // valid and unaliased until this present completes.
            vk_info.p_next = unsafe { chain.splice_into(vk_info.p_next) };
        }

        let suboptimal = {
            profiling::scope!("vkQueuePresentKHR");
            unsafe { self.functor.queue_present(queue.raw, &vk_info) }.map_err(|error| {
                match error {
                    vk::Result::ERROR_OUT_OF_DATE_KHR => crate::SurfaceError::Outdated,
                    vk::Result::ERROR_SURFACE_LOST_KHR => crate::SurfaceError::Lost,
                    // We don't use VK_EXT_full_screen_exclusive
                    // VK_ERROR_FULL_SCREEN_EXCLUSIVE_MODE_LOST_EXT
                    _ => map_host_device_oom_and_lost_err(error).into(),
                }
            })?
        };
        if suboptimal {
            // We treat `VK_SUBOPTIMAL_KHR` as `VK_SUCCESS` on Android.
            // On Android 10+, libvulkan's `vkQueuePresentKHR` implementation returns `VK_SUBOPTIMAL_KHR` if not doing pre-rotation
            // (i.e `VkSwapchainCreateInfoKHR::preTransform` not being equal to the current device orientation).
            // This is always the case when the device orientation is anything other than the identity one, as we unconditionally use `VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR`.
            #[cfg(not(target_os = "android"))]
            log::debug!("Suboptimal present of frame {}", texture.index);
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl NativeSwapchain {
    unsafe fn present_shared(
        &mut self,
        queue: &crate::vulkan::Queue,
        texture: crate::vulkan::SurfaceTexture,
    ) -> Result<(), crate::SurfaceError> {
        let generation = self.generation.as_ref().unwrap();
        let metadata = texture
            .metadata
            .as_any()
            .downcast_ref::<NativeSurfaceTextureMetadata>()
            .ok_or(crate::SurfaceError::Other(
                "Incompatible shared surface texture",
            ))?;
        if !metadata
            .generation
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, generation))
        {
            return Err(crate::SurfaceError::Other("Stale shared surface"));
        }
        generation.validate_image(queue, texture.index, texture.texture.identity)?;
        if self.next_present_chain.is_some() {
            generation.close();
            return Err(crate::SurfaceError::Other(
                "Unsupported shared present chain",
            ));
        }
        let present_fence = match generation.prepare_present() {
            Ok(fence) => fence,
            Err(error) => {
                generation.close();
                return Err(error.into());
            }
        };
        let mut acquire = metadata.acquire_semaphores.lock();
        let mut present = metadata.present_semaphores.lock();
        let waits = present.get_present_wait_semaphores();
        let swapchains = [self.raw];
        let indices = [texture.index];
        let fences = [present_fence.raw()];
        let mut completion = vk::SwapchainPresentFenceInfoEXT::default().fences(&fences);
        let mut info = vk::PresentInfoKHR::default()
            .swapchains(&swapchains)
            .image_indices(&indices)
            .wait_semaphores(&waits)
            .push_next(&mut completion);
        let times = self.next_present_time.take().map(|time| [time]);
        let mut timing;
        if let Some(times) = &times {
            timing = vk::PresentTimesInfoGOOGLE::default().times(times);
            info = info.push_next(&mut timing);
        }
        let result = unsafe { self.functor.queue_present(queue.raw, &info) };
        #[cfg(test)]
        let injected = result.is_ok()
            && queue
                .device
                .shared_queue_group
                .as_ref()
                .unwrap()
                .present_after_native_error
                .swap(false, core::sync::atomic::Ordering::AcqRel);
        #[cfg(test)]
        let result = if injected {
            Err(vk::Result::ERROR_UNKNOWN)
        } else {
            result
        };
        let accepted = matches!(
            result,
            Ok(_) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR | vk::Result::ERROR_SURFACE_LOST_KHR)
        );
        if accepted {
            generation.presented(present_fence, false);
            acquire.end_semaphore_usage();
            present.end_semaphore_usage();
            generation.finish_image(texture.index);
        } else if matches!(
            result,
            Err(vk::Result::ERROR_OUT_OF_HOST_MEMORY | vk::Result::ERROR_OUT_OF_DEVICE_MEMORY)
        ) {
            drop(present_fence);
        } else {
            generation.presented(present_fence, true);
            queue
                .device
                .shared_queue_group
                .as_ref()
                .unwrap()
                .close_uncertain();
        }
        if result.is_err() {
            generation.close();
        }
        match result {
            Ok(_) => Ok(()),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Err(crate::SurfaceError::Outdated),
            Err(vk::Result::ERROR_SURFACE_LOST_KHR) => Err(crate::SurfaceError::Lost),
            Err(error) => {
                #[cfg(test)]
                if injected {
                    return Err(crate::DeviceError::Unexpected.into());
                }
                Err(map_host_device_oom_and_lost_err(error).into())
            }
        }
    }

    pub(crate) fn belongs_to(&self, device: &crate::vulkan::Device) -> bool {
        self.device.raw.handle() == device.shared.raw.handle()
            && self
                .device
                .shared_queue_group
                .as_ref()
                .zip(device.shared.shared_queue_group.as_ref())
                .is_some_and(|(current, requested)| Arc::ptr_eq(current, requested))
    }

    pub(crate) fn shared_generation(&self) -> Option<Arc<Generation>> {
        self.generation.clone()
    }

    pub(crate) fn as_raw(&self) -> vk::SwapchainKHR {
        self.raw
    }

    pub fn set_next_present_time(&mut self, present_timing: vk::PresentTimeGOOGLE) {
        if self
            .device
            .enabled_extensions
            .contains(&google::display_timing::NAME)
        {
            self.next_present_time = Some(present_timing);
        } else {
            panic!(
                "Tried to set display timing properties without the VK_GOOGLE_display_timing extension enabled."
            );
        }
    }

    /// # Safety
    ///
    /// See [`Surface::set_next_present_chain()`](crate::vulkan::Surface::set_next_present_chain).
    pub unsafe fn set_next_present_chain(&mut self, chain: *mut core::ffi::c_void) {
        self.next_present_chain = Some(PnextChain::new(chain));
    }

    /// Mark the current frame finished, advancing to the next acquire semaphore.
    fn advance_acquire_semaphore(&mut self) {
        let semaphore_count = self.acquire_semaphores.len();
        self.next_acquire_index = (self.next_acquire_index + 1) % semaphore_count;
    }

    /// Get the next acquire semaphore that should be used with this swapchain.
    fn get_acquire_semaphore(&self) -> Arc<Mutex<SwapchainAcquireSemaphore>> {
        self.acquire_semaphores[self.next_acquire_index].clone()
    }

    /// Get the set of present semaphores that should be used with the given image index.
    fn get_present_semaphores(&self, index: u32) -> Arc<Mutex<SwapchainPresentSemaphores>> {
        self.present_semaphores[index as usize].clone()
    }
}

/// Semaphore used to acquire a swapchain image.
#[derive(Debug)]
struct SwapchainAcquireSemaphore {
    /// A semaphore that is signaled when this image is safe for us to modify.
    ///
    /// When [`vkAcquireNextImageKHR`] returns the index of the next swapchain
    /// image that we should use, that image may actually still be in use by the
    /// presentation engine, and is not yet safe to modify. However, that
    /// function does accept a semaphore that it will signal when the image is
    /// indeed safe to begin messing with.
    ///
    /// This semaphore is:
    ///
    /// - waited for by the first queue submission to operate on this image
    ///   since it was acquired, and
    ///
    /// - signaled by [`vkAcquireNextImageKHR`] when the acquired image is ready
    ///   for us to use.
    ///
    /// [`vkAcquireNextImageKHR`]: https://registry.khronos.org/vulkan/specs/1.3-extensions/html/vkspec.html#vkAcquireNextImageKHR
    acquire: vk::Semaphore,

    /// True if the next command submission operating on this image should wait
    /// for [`acquire`].
    ///
    /// We must wait for `acquire` before drawing to this swapchain image, but
    /// because `wgpu-hal` queue submissions are always strongly ordered, only
    /// the first submission that works with a swapchain image actually needs to
    /// wait. We set this flag when this image is acquired, and clear it the
    /// first time it's passed to [`Queue::submit`] as a surface texture.
    ///
    /// Additionally, semaphores can only be waited on once, so we need to ensure
    /// that we only actually pass this semaphore to the first submission that
    /// uses that image.
    ///
    /// [`acquire`]: SwapchainAcquireSemaphore::acquire
    /// [`Queue::submit`]: crate::Queue::submit
    should_wait_for_acquire: bool,

    /// The fence value of the last command submission that wrote to this image.
    ///
    /// The next time we try to acquire this image, we'll block until
    /// this submission finishes, proving that [`acquire`] is ready to
    /// pass to `vkAcquireNextImageKHR` again.
    ///
    /// [`acquire`]: SwapchainAcquireSemaphore::acquire
    previously_used_submission_index: crate::FenceValue,
}

impl SwapchainAcquireSemaphore {
    fn new(device: &DeviceShared, index: usize) -> Result<Self, crate::DeviceError> {
        Ok(Self {
            acquire: device
                .new_binary_semaphore(&format!("SwapchainImageSemaphore: Index {index} acquire"))?,
            should_wait_for_acquire: true,
            previously_used_submission_index: 0,
        })
    }

    /// Sets the fence value which the next acquire will wait for. This prevents
    /// the semaphore from being used while the previous submission is still in flight.
    fn set_used_fence_value(&mut self, value: crate::FenceValue) {
        self.previously_used_submission_index = value;
    }

    /// Return the semaphore that commands drawing to this image should wait for, if any.
    ///
    /// This only returns `Some` once per acquisition; see
    /// [`SwapchainAcquireSemaphore::should_wait_for_acquire`] for details.
    fn get_acquire_wait_semaphore(&mut self) -> Option<vk::Semaphore> {
        if self.should_wait_for_acquire {
            self.should_wait_for_acquire = false;
            Some(self.acquire)
        } else {
            None
        }
    }

    /// Indicates the cpu-side usage of this semaphore has finished for the frame,
    /// so reset internal state to be ready for the next frame.
    fn end_semaphore_usage(&mut self) {
        // Reset the acquire semaphore, so that the next time we acquire this
        // image, we can wait for it again.
        self.should_wait_for_acquire = true;
    }

    unsafe fn destroy(&self, device: &ash::Device) {
        unsafe {
            device.destroy_semaphore(self.acquire, None);
        }
    }
}

#[derive(Debug)]
struct SwapchainPresentSemaphores {
    /// A pool of semaphores for ordering presentation after drawing.
    ///
    /// The first [`present_index`] semaphores in this vector are:
    ///
    /// - all waited on by the call to [`vkQueuePresentKHR`] that presents this
    ///   image, and
    ///
    /// - each signaled by some [`vkQueueSubmit`] queue submission that draws to
    ///   this image, when the submission finishes execution.
    ///
    /// This vector accumulates one semaphore per submission that writes to this
    /// image. This is awkward, but hard to avoid: [`vkQueuePresentKHR`]
    /// requires a semaphore to order it with respect to drawing commands, and
    /// we can't attach new completion semaphores to a command submission after
    /// it's been submitted. This means that, at submission time, we must create
    /// the semaphore we might need if the caller's next action is to enqueue a
    /// presentation of this image.
    ///
    /// An alternative strategy would be for presentation to enqueue an empty
    /// submit, ordered relative to other submits in the usual way, and
    /// signaling a single presentation semaphore. But we suspect that submits
    /// are usually expensive enough, and semaphores usually cheap enough, that
    /// performance-sensitive users will avoid making many submits, so that the
    /// cost of accumulated semaphores will usually be less than the cost of an
    /// additional submit.
    ///
    /// Only the first [`present_index`] semaphores in the vector are actually
    /// going to be signalled by submitted commands, and need to be waited for
    /// by the next present call. Any semaphores beyond that index were created
    /// for prior presents and are simply being retained for recycling.
    ///
    /// [`present_index`]: SwapchainPresentSemaphores::present_index
    /// [`vkQueuePresentKHR`]: https://registry.khronos.org/vulkan/specs/1.3-extensions/html/vkspec.html#vkQueuePresentKHR
    /// [`vkQueueSubmit`]: https://registry.khronos.org/vulkan/specs/1.3-extensions/html/vkspec.html#vkQueueSubmit
    present: Vec<vk::Semaphore>,

    /// The number of semaphores in [`present`] to be signalled for this submission.
    ///
    /// [`present`]: SwapchainPresentSemaphores::present
    present_index: usize,

    /// Which image this semaphore set is used for.
    frame_index: usize,
}

impl SwapchainPresentSemaphores {
    pub fn new(frame_index: usize) -> Self {
        Self {
            present: Vec::new(),
            present_index: 0,
            frame_index,
        }
    }

    /// Return the semaphore that the next submission that writes to this image should
    /// signal when it's done.
    ///
    /// See [`SwapchainPresentSemaphores::present`] for details.
    fn get_submit_signal_semaphore(
        &mut self,
        device: &DeviceShared,
    ) -> Result<vk::Semaphore, crate::DeviceError> {
        // Try to recycle a semaphore we created for a previous presentation.
        let sem = match self.present.get(self.present_index) {
            Some(sem) => *sem,
            None => {
                let sem = device.new_binary_semaphore(&format!(
                    "SwapchainImageSemaphore: Image {} present semaphore {}",
                    self.frame_index, self.present_index
                ))?;
                self.present.push(sem);
                sem
            }
        };

        self.present_index += 1;

        Ok(sem)
    }

    /// Indicates the cpu-side usage of this semaphore has finished for the frame,
    /// so reset internal state to be ready for the next frame.
    fn end_semaphore_usage(&mut self) {
        // Reset the index to 0, so that the next time we get a semaphore, we
        // start from the beginning of the list.
        self.present_index = 0;
    }

    /// Return the semaphores that a presentation of this image should wait on.
    ///
    /// Return a slice of semaphores that the call to [`vkQueueSubmit`] that
    /// ends this image's acquisition should wait for. See
    /// [`SwapchainPresentSemaphores::present`] for details.
    ///
    /// Reset `self` to be ready for the next acquisition cycle.
    ///
    /// [`vkQueueSubmit`]: https://registry.khronos.org/vulkan/specs/1.3-extensions/html/vkspec.html#vkQueueSubmit
    fn get_present_wait_semaphores(&mut self) -> Vec<vk::Semaphore> {
        self.present[0..self.present_index].to_vec()
    }

    unsafe fn destroy(&self, device: &ash::Device) {
        unsafe {
            for sem in &self.present {
                device.destroy_semaphore(*sem, None);
            }
        }
    }
}

#[derive(Debug)]
struct NativeSurfaceTextureMetadata {
    acquire_semaphores: Arc<Mutex<SwapchainAcquireSemaphore>>,
    present_semaphores: Arc<Mutex<SwapchainPresentSemaphores>>,
    // Keep the generation until its semaphore leases drop.
    generation: Option<Arc<Generation>>,
}

impl SurfaceTextureMetadata for NativeSurfaceTextureMetadata {
    fn get_semaphore_guard(&self) -> Box<dyn SwapchainSubmissionSemaphoreGuard + '_> {
        Box::new(NativeSwapchainSubmissionSemaphoreGuard {
            acquire_semaphore_guard: self
                .acquire_semaphores
                .try_lock()
                .expect("Failed to lock surface acquire semaphore"),
            present_semaphores_guard: self
                .present_semaphores
                .try_lock()
                .expect("Failed to lock surface present semaphores"),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct NativeSwapchainSubmissionSemaphoreGuard<'a> {
    acquire_semaphore_guard: MutexGuard<'a, SwapchainAcquireSemaphore>,
    present_semaphores_guard: MutexGuard<'a, SwapchainPresentSemaphores>,
}

impl<'a> SwapchainSubmissionSemaphoreGuard for NativeSwapchainSubmissionSemaphoreGuard<'a> {
    fn set_used_fence_value(&mut self, value: u64) {
        self.acquire_semaphore_guard.set_used_fence_value(value);
    }

    fn get_acquire_wait_semaphore(&mut self) -> Option<SemaphoreType> {
        self.acquire_semaphore_guard
            .get_acquire_wait_semaphore()
            .map(SemaphoreType::Binary)
    }

    fn get_submit_signal_semaphore(
        &mut self,
        device: &DeviceShared,
    ) -> Result<SemaphoreType, crate::DeviceError> {
        self.present_semaphores_guard
            .get_submit_signal_semaphore(device)
            .map(SemaphoreType::Binary)
    }
}

pub(crate) fn validate_shared_submission(
    queue: &crate::vulkan::Queue,
    textures: &[&crate::vulkan::SurfaceTexture],
    fence: &crate::vulkan::Fence,
) -> Result<Vec<Arc<Generation>>, crate::DeviceError> {
    let mut generations = Vec::new();
    for texture in textures {
        let metadata = texture
            .metadata
            .as_any()
            .downcast_ref::<NativeSurfaceTextureMetadata>()
            .ok_or(crate::DeviceError::Unexpected)?;
        let generation = metadata
            .generation
            .as_ref()
            .ok_or(crate::DeviceError::Unexpected)?;
        generation.validate_image(queue, texture.index, texture.texture.identity)?;
        generation.validate_fence(fence)?;
        if generations
            .iter()
            .any(|previous| Arc::ptr_eq(previous, generation))
        {
            return Err(crate::DeviceError::Unexpected);
        }
        generations.push(generation.clone());
    }
    Ok(generations)
}
