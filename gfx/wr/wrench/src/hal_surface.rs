/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::wrench::Wrench;
use std::{rc::Rc, time::{Duration, Instant}};
use webrender::api::*;
use webrender::api::units::*;
use webrender::hal::{Options, PresentationStatus};
use webrender::render_api::Transaction;
use winit::{application::ApplicationHandler, event::WindowEvent, event_loop::{ActiveEventLoop, ControlFlow, EventLoop}, window::{Window, WindowId}};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

pub fn run(options: Options) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|error| error.to_string())?;
    let mut app = Probe { options, window: None, wrench: None, frame: 0,
        #[cfg(feature = "hal-testing")]
        failure_index: 0,
        ready_at: None, deadline: Instant::now() + Duration::from_secs(30), error: None };
    event_loop.run_app(&mut app).map_err(|error| error.to_string())?;
    if let Some(wrench) = app.wrench.take() { wrench.api.shut_down(true); drop(wrench); }
    if let Some(error) = app.error { return Err(error); }
    if app.frame < 12 { return Err("Surface probe ended before completing its assertions".into()); }
    Ok(())
}

struct Probe {
    options: Options,
    #[cfg(feature = "hal-testing")]
    failure_index: usize,
    window: Option<Rc<Window>>,
    wrench: Option<Wrench<webrender::hal::Renderer>>,
    frame: u32,
    ready_at: Option<Instant>,
    deadline: Instant,
    error: Option<String>,
}

impl Probe {
    fn draw(&mut self) -> Result<(), String> {
        #[cfg(feature = "hal-testing")]
        if self.frame == 1 && std::env::var_os("WR_HAL_SURFACE_FAILURES").is_some() {
            use webrender::hal::FailurePoint;
            let points = [FailurePoint::Acquire, FailurePoint::Configure, FailurePoint::Submit, FailurePoint::Record, FailurePoint::Map];
            if let Some(&point) = points.get(self.failure_index) {
                let wrench = self.wrench.as_mut().unwrap();
                let rect = FramebufferIntRect::from_size(FramebufferIntSize::new(128, 96));
                let pending = wrench.renderer.request_readback(rect)?;
                if point == FailurePoint::Submit { assert_eq!(wrench.renderer.acquire_surface()?, PresentationStatus::Acquired); }
                wrench.renderer.inject_failure(point);
                let result = match point {
                    FailurePoint::Acquire => wrench.renderer.acquire_surface().map(|_| ()),
                    FailurePoint::Configure => wrench.renderer.resize_surface([128, 96]),
                    FailurePoint::Submit => wrench.renderer.present().map(|_| ()),
                    FailurePoint::Record => wrench.renderer.render().map(|_| ()),
                    FailurePoint::Map => wrench.renderer.read_pixels_rgba8(rect).map(|_| ()),
                    _ => unreachable!(),
                };
                assert!(result.is_err());
                assert!(wrench.renderer.is_failed());
                assert!(wrench.renderer.wait_readback(pending).is_err());
                assert!(wrench.renderer.render().is_err());
                let old = self.wrench.take().unwrap();
                old.api.shut_down(true);
                drop(old);
                self.wrench = Some(Wrench::new_hal_for_window(&self.options, DeviceIntSize::new(128, 96), self.window.as_ref().unwrap().clone(), Default::default())?);
                self.failure_index += 1;
                self.frame = 0;
                eprintln!("HAL FAILURE recreated after {point:?}");
                return Ok(());
            }
        }
        let wrench = self.wrench.as_mut().unwrap();
        let window = self.window.as_ref().unwrap();
        let size = window.inner_size();
        let size = DeviceIntSize::new(size.width as i32, size.height as i32);
        if self.frame == 2 || self.frame == 4 {
            for _ in 0..3 {
                if wrench.renderer.acquire_surface()? != PresentationStatus::Acquired { return Err("Surface discard probe could not acquire".into()); }
                assert!(wrench.renderer.acquire_surface().is_err());
                wrench.renderer.discard_surface()?;
            }
        }
        if self.frame == 6 {
            wrench.renderer.resize_surface([0, 0])?;
            assert_eq!(wrench.renderer.acquire_surface()?, PresentationStatus::Suspended);
        }
        if self.frame == 6 || wrench.renderer.surface_info().unwrap().size != [size.width as u32, size.height as u32] {
            wrench.renderer.resize_surface([size.width as u32, size.height as u32])?;
        }
        let mut txn = Transaction::new();
        txn.set_document_view(DeviceIntRect::from_size(size));
        let mut builder = DisplayListBuilder::new(wrench.root_pipeline_id);
        builder.begin(crate::AU_PER_DEV_PX);
        let rect = LayoutRect::from_size(LayoutSize::new(size.width as f32, size.height as f32));
        let info = CommonItemProperties { clip_rect: rect, clip_chain_id: ClipChainId::INVALID,
            spatial_id: SpatialId::root_scroll_node(wrench.root_pipeline_id), flags: PrimitiveFlags::default() };
        builder.push_rect(&info, rect, ColorF::new(1.0, 0.0, 0.0, 1.0));
        builder.push_rect(&info, LayoutRect::from_origin_and_size(LayoutPoint::new(0.0, size.height as f32 / 2.0),
            LayoutSize::new(size.width as f32, size.height as f32 / 2.0)), ColorF::new(0.0, 0.0, 1.0, 1.0));
        builder.push_rect(&info, LayoutRect::from_size(LayoutSize::new(size.width as f32 / 4.0, size.height as f32)),
            ColorF::new(64.0 / 255.0, 128.0 / 255.0, 192.0 / 255.0, 1.0));
        txn.set_display_list(Epoch(self.frame), wrench.api.get_namespace_id(), builder.end());
        txn.generate_frame(self.frame as u64, true, false, RenderReasons::TESTING);
        wrench.api.send_transaction(wrench.document_id, txn);
        wrench.renderer.prepare_frame(wrench.document_id)?;
        wrench.renderer.render()?;
        if !matches!(wrench.renderer.present()?, PresentationStatus::Presented { .. }) { return Err("Surface frame was not presented".into()); }
        wrench.renderer.poll()?;
        self.frame += 1;
        if self.frame == 3 { let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(137, 99)); }
        if self.frame == 7 { let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(128, 96)); }
        if self.frame == 12 {
            let info = wrench.renderer.surface_info().unwrap();
            assert_eq!(info.presented, 12);
            assert_eq!(info.discarded, 6);
            assert_eq!(info.acquired, info.presented + info.discarded);
            assert!(info.generation >= 4);
            let id = match window.window_handle().map_err(|error| error.to_string())?.as_raw() {
                RawWindowHandle::Xlib(handle) => handle.window as u64,
                RawWindowHandle::Xcb(handle) => handle.window.get() as u64,
                _ => 0,
            };
            eprintln!("HAL SURFACE READY window={id} width={} height={} info={info:?}", size.width, size.height);
            self.ready_at = Some(Instant::now());
        }
        Ok(())
    }
}

impl ApplicationHandler for Probe {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() { return; }
        let result = (|| {
            let window = Rc::new(event_loop.create_window(Window::default_attributes().with_title("WR Vulkan surface probe")
                .with_inner_size(winit::dpi::PhysicalSize::new(128, 96))).map_err(|error| error.to_string())?);
            let wrench = Wrench::new_hal_for_window(&self.options, DeviceIntSize::new(128, 96), window.clone(), Default::default())?;
            eprintln!("HAL SURFACE adapter={} backend={:?}", wrench.renderer.info().name, wrench.renderer.info().backend);
            window.request_redraw();
            self.window = Some(window);
            self.wrench = Some(wrench);
            Ok::<_, String>(())
        })();
        if let Err(error) = result { self.error = Some(error); event_loop.exit(); }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::RedrawRequested) && self.frame < 12 && self.wrench.is_some() {
            if let Err(error) = self.draw() { self.error = Some(error); event_loop.exit(); }
        }
        if matches!(event, WindowEvent::CloseRequested) { event_loop.exit(); }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if Instant::now() > self.deadline { self.error = Some("Surface probe timed out".into()); event_loop.exit(); return; }
        if self.ready_at.map_or(false, |time| time.elapsed() >= Duration::from_secs(2)) {
            match self.wrench.as_mut().unwrap().renderer.acquire_surface() {
                Ok(PresentationStatus::Acquired) => eprintln!("HAL SURFACE shutdown with an acquired image"),
                result => self.error = Some(format!("Teardown acquisition failed: {result:?}")),
            }
            event_loop.exit();
            return;
        }
        if self.frame < 12 { if let Some(window) = &self.window { window.request_redraw(); } }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(20)));
    }
}
