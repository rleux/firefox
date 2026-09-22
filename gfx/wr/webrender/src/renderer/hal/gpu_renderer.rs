/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::hal::render::gpu_backend::HalGpuBackend;
use std::ops::{Deref, DerefMut};

pub(super) struct GpuRenderer<A: BackendApi> {
    pub inner: Option<crate::Renderer>,
    marker: std::marker::PhantomData<A>,
}

impl<A: BackendApi> GpuRenderer<A> {
    pub fn new(
        gpu: FrameRenderer<A>,
        mut options: WebRenderOptions,
        notifier: Box<dyn RenderNotifier>,
    ) -> Result<(Self, RenderApiSender), String> {
        let mut backend = HalGpuBackend::from_renderer(gpu)?;
        options.compositor_config = backend.compositor_config();
        let config = crate::device::GpuBackendConfig::Hal(Box::new(backend));
        let (renderer, sender) = crate::create_webrender_instance(config, notifier, options, None)
            .map_err(|error| format!("Creating HAL renderer: {error:?}"))?;
        Ok((
            Self {
                inner: Some(renderer),
                marker: std::marker::PhantomData,
            },
            sender,
        ))
    }

    pub fn set_external_image_provider(
        &mut self,
        provider: Box<dyn crate::device::hal::ExternalImageProvider>,
    ) {
        let renderer = self.inner.as_mut().unwrap();
        let handler = renderer
            .device
            .backend_mut::<HalGpuBackend<A>>()
            .external_handler(provider);
        renderer.set_external_image_handler(handler);
    }

    #[cfg(feature = "capture")]
    pub fn save_capture(
        &mut self,
        config: crate::capture::CaptureConfig,
        externals: Vec<crate::capture::ExternalCaptureImage>,
        _: Option<api::units::DeviceIntSize>,
    ) -> Result<(), String> {
        if self
            .inner
            .as_ref()
            .unwrap()
            .device
            .backend::<HalGpuBackend<A>>()
            .uses_native_compositor()
        {
            let size = self.inner.as_ref().unwrap().device_size;
            self.deref_mut()
                .save_capture(config.clone(), Vec::new(), size)?;
        }
        if self.is_failed() {
            return Err("Cannot capture a failed HAL renderer".into());
        }
        self.begin_capture_metadata(&config)?;
        self.inner
            .as_mut()
            .unwrap()
            .save_capture(config.clone(), externals);
        if let Some(error) = self
            .inner
            .as_ref()
            .unwrap()
            .device
            .backend::<HalGpuBackend<A>>()
            .failure()
        {
            return Err(error);
        }
        self.finish_capture_metadata(&config)
    }

    #[cfg(feature = "replay")]
    pub fn load_capture(
        &mut self,
        config: crate::capture::CaptureConfig,
        externals: Vec<crate::capture::PlainExternalImage>,
    ) -> Result<(), String> {
        if self.is_failed() {
            return Err("Cannot replay on a failed HAL renderer".into());
        }
        self.validate_capture(&config)?;
        if self
            .inner
            .as_ref()
            .unwrap()
            .device
            .backend::<HalGpuBackend<A>>()
            .uses_native_compositor()
        {
            let images = externals
                .iter()
                .map(|image| crate::capture::PlainExternalImage {
                    data: image.data.clone(),
                    external: image.external,
                    uv: image.uv,
                })
                .collect();
            self.deref_mut().load_capture(config.clone(), images)?;
        }
        self.inner.as_mut().unwrap().load_capture(config, externals);
        Ok(())
    }

    pub fn update_resources(&mut self, updates: Vec<ResourceUpdateList>) -> Result<(), String> {
        let renderer = self.inner.as_mut().unwrap();
        for update in updates {
            renderer.pending_texture_cache_updates |= !update.texture_updates.updates.is_empty();
            renderer
                .pending_texture_updates
                .push(update.texture_updates);
            renderer
                .pending_native_surface_updates
                .extend(update.native_surface_updates);
        }
        Ok(())
    }

    pub fn notify_texture_update(&mut self, request: NotificationRequest) {
        let renderer = self.inner.as_mut().unwrap();
        if renderer.pending_texture_cache_updates {
            renderer.notifications.push(request);
        } else {
            request.notify();
        }
    }

    pub fn flush_resources(&mut self) -> Result<(), String> {
        let renderer = self.inner.as_mut().unwrap();
        renderer.device.begin_frame();
        renderer.update_texture_cache();
        renderer.update_native_surfaces();
        renderer.device.end_frame();
        match renderer.device.backend::<HalGpuBackend<A>>().failure() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn render_document(
        &mut self,
        id: DocumentId,
        document: &mut RenderedDocument,
        present: bool,
        damage: Option<api::units::DeviceIntRect>,
    ) -> Result<RenderedFrame<A>, String> {
        if self.is_failed() {
            return Err(self
                .inner
                .as_ref()
                .unwrap()
                .device
                .backend::<HalGpuBackend<A>>()
                .failure()
                .unwrap_or_else(|| "HAL renderer requires recreation".into()));
        }
        if let Some(metrics) = self.metrics() {
            use crate::device::hal::diagnostics::RenderCounter;
            metrics.add(RenderCounter::Executions, 1);
            if present {
                metrics.add(
                    if damage.is_some() {
                        RenderCounter::PartialCompositions
                    } else {
                        RenderCounter::FullCompositions
                    },
                    1,
                );
                let rect = damage.unwrap_or(document.frame.device_rect);
                metrics.add(
                    RenderCounter::ComposedPixels,
                    rect.width() as u64 * rect.height() as u64,
                );
            } else {
                metrics.add(RenderCounter::OffscreenExecutions, 1);
            }
            if !document.frame.has_been_rendered {
                let tiles: usize = document
                    .frame
                    .passes
                    .iter()
                    .map(|pass| pass.picture_cache.len())
                    .sum();
                metrics.add(RenderCounter::RasterizedTiles, tiles as u64);
            }
        }
        let size = present.then_some(document.frame.device_rect.size());
        let renderer = self.inner.as_mut().unwrap();
        renderer
            .device
            .backend::<HalGpuBackend<A>>()
            .configure_external_images(&document.frame);
        renderer
            .device
            .backend_mut::<HalGpuBackend<A>>()
            .output_origin = document.frame.device_rect.min;
        renderer
            .device
            .backend_mut::<HalGpuBackend<A>>()
            .composition_damage =
            damage.map(|rect| rect.translate(-document.frame.device_rect.min.to_vector()));
        renderer.device_size = size;
        let results = renderer
            .render_impl(id, document, size, 0)
            .map_err(|error| {
                renderer
                    .device
                    .backend::<HalGpuBackend<A>>()
                    .failure()
                    .unwrap_or_else(|| format!("Rendering HAL frame: {error:?}"))
            })?;
        let stats = crate::device::hal::DrawStats {
            wr_draw_calls: results.stats.total_draw_calls,
            draw_calls: results.stats.total_draw_calls,
            color_targets: results.stats.color_target_count,
            alpha_targets: results.stats.alpha_target_count,
            ..Default::default()
        };
        if present
            && renderer
                .device
                .backend::<HalGpuBackend<A>>()
                .uses_native_compositor()
        {
            let textures: Vec<_> = renderer
                .texture_resolver
                .texture_cache_map
                .iter()
                .map(|(&key, entry)| (key, &entry.texture))
                .collect();
            let backend = renderer.device.backend_mut::<HalGpuBackend<A>>();
            backend.set_compositor_textures(&textures);
            let mut output =
                backend.compositor_output(&mut document.frame, renderer.clear_color)?;
            output.stats.wr_draw_calls += stats.wr_draw_calls;
            Ok(output)
        } else {
            renderer.device.backend_mut::<HalGpuBackend<A>>().output(
                document.frame.device_rect.min,
                stats,
                present,
            )
        }
    }
}

impl<A: BackendApi> Deref for GpuRenderer<A> {
    type Target = FrameRenderer<A>;
    fn deref(&self) -> &Self::Target {
        &self
            .inner
            .as_ref()
            .unwrap()
            .device
            .backend::<HalGpuBackend<A>>()
            .gpu
    }
}

impl<A: BackendApi> DerefMut for GpuRenderer<A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self
            .inner
            .as_mut()
            .unwrap()
            .device
            .backend_mut::<HalGpuBackend<A>>()
            .gpu
    }
}

impl<A: BackendApi> Drop for GpuRenderer<A> {
    fn drop(&mut self) {
        if let Some(renderer) = self.inner.take() {
            renderer.deinit();
        }
    }
}
