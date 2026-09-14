/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::wrench::{Wrench, WrenchThing};
use std::{collections::HashMap, path::{Path, PathBuf}, rc::Rc, time::{Duration, Instant}};
use webrender::api::*;
use webrender::api::units::*;
use webrender::hal::{Options, PresentationStatus, RecordedFrameHandle, Renderer, SurfaceOptions};
use webrender::render_api::{CaptureBits, ClearCache, DebugCommand, Transaction};
use winit::{application::ApplicationHandler, dpi::PhysicalSize, event::{ElementState, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy}, keyboard::{Key, NamedKey}, window::{Window, WindowId}};

#[derive(Clone)]
struct Wake { window: WindowId, generation: u64, composite: bool }

struct Notifier { window: WindowId, generation: u64, proxy: EventLoopProxy<Wake> }
impl RenderNotifier for Notifier {
    fn clone(&self) -> Box<dyn RenderNotifier> { Box::new(Self { window: self.window, generation: self.generation, proxy: self.proxy.clone() }) }
    fn wake_up(&self, composite: bool) { let _ = self.proxy.send_event(Wake { window: self.window, generation: self.generation, composite }); }
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, params: &FrameReadyParams) { self.wake_up(params.present); }
}

struct Screenshot { handle: RecordedFrameHandle, size: DeviceIntSize, pixels: Vec<u8>, path: PathBuf }

struct Pane {
    window: Rc<Window>,
    wrench: Wrench<Renderer>,
    thing: Box<dyn WrenchThing<Renderer>>,
    number: u64,
    size: DeviceIntSize,
    scale: f32,
    pending_size: Option<DeviceIntSize>,
    pending_scale: Option<f32>,
    occluded: bool,
    minimized: bool,
    suspended: bool,
    needs_update: bool,
    do_frame: bool,
    pending_frame: bool,
    redraw: bool,
    looping: bool,
    frames: u64,
    retry_at: Instant,
    cursor: WorldPoint,
    screenshots: Vec<Screenshot>,
    gpu_timing: bool,
}

impl Pane {
    fn hidden(&self) -> bool { self.suspended || self.occluded || self.minimized || self.size.is_empty() }

    fn update_view(&mut self) -> Result<(), String> {
        let size = self.pending_size.take().unwrap_or(self.size);
        let scale = self.pending_scale.take().unwrap_or(self.scale);
        self.minimized = self.window.is_minimized().unwrap_or(false);
        if size != self.size || scale != self.scale {
            self.size = size;
            self.scale = scale;
            self.wrench.update(size);
            self.thing.on_window_changed(size, scale);
            let mut txn = Transaction::new();
            txn.set_document_view(DeviceIntRect::from_size(size));
            self.wrench.api.send_transaction(self.wrench.document_id, txn);
            self.do_frame = true;
            self.needs_update = true;
            eprintln!("HAL WINDOW resized number={} size={}x{} scale={scale}", self.number, size.width, size.height);
        }
        let target = if self.hidden() { [0, 0] } else { [size.width as u32, size.height as u32] };
        if self.wrench.renderer.surface_info().unwrap().size != target {
            self.wrench.renderer.resize_surface(target)?;
            if !self.hidden() { self.do_frame = true; self.redraw = true; }
            eprintln!("HAL WINDOW visibility number={} hidden={}", self.number, self.hidden());
        }
        Ok(())
    }

    fn tick(&mut self, limit: Option<u64>, no_block: bool, watch: bool, verbose: bool) -> Result<bool, String> {
        self.update_view()?;
        if self.needs_update {
            self.wrench.renderer.update()?;
            self.needs_update = false;
            if self.wrench.renderer.prepare_frame_if_ready(self.wrench.document_id)?.is_some() {
                self.pending_frame = false;
                self.redraw = true;
            }
            if verbose {
                eprintln!("HAL WINDOW serviced number={} hidden={} frame={}", self.number, self.hidden(), self.wrench.renderer.has_frame());
            }
        }
        self.wrench.renderer.poll()?;
        let mut index = 0;
        while index < self.screenshots.len() {
            let shot = &mut self.screenshots[index];
            if self.wrench.renderer.map_recorded_frame(shot.handle, &mut shot.pixels, shot.size.width as usize * 4)? {
                let shot = self.screenshots.remove(index);
                crate::png::save(&shot.path, shot.pixels, shot.size, crate::png::SaveSettings { flip_vertical: false, try_crop: false });
                eprintln!("HAL WINDOW screenshot number={} size={}x{} path={}", self.number, shot.size.width, shot.size.height, shot.path.display());
            } else { index += 1; }
        }
        if self.gpu_timing {
            for timing in self.wrench.renderer.take_gpu_timings()? { println!("HAL GPU window={} ns={}", self.number, timing.nanoseconds); }
        }
        if self.hidden() { return Ok(false); }
        if watch && !self.pending_frame && !self.redraw && Instant::now() >= self.retry_at { self.do_frame = true; }
        if self.do_frame && !self.pending_frame {
            self.thing.do_frame(&mut self.wrench);
            self.do_frame = false;
            self.pending_frame = true;
        }
        if self.redraw && !self.pending_frame && self.wrench.renderer.has_frame() && Instant::now() >= self.retry_at {
            self.wrench.renderer.render()?;
            if !self.wrench.renderer.has_presentable_output() {
                self.redraw = false;
                return Ok(false);
            }
            let status = self.wrench.renderer.acquire_surface()?;
            let status = if status == PresentationStatus::Acquired {
                self.window.pre_present_notify();
                self.wrench.renderer.present()?
            } else { status };
            if let PresentationStatus::Presented { .. } = status {
                self.redraw = false;
                self.frames += 1;
                if self.frames == 1 || verbose { eprintln!("HAL WINDOW presented number={} frame={}", self.number, self.frames); }
                if limit.map_or(false, |limit| self.frames >= limit) { return Ok(true); }
                if self.looping || limit.is_some() { self.thing.next_frame(); self.do_frame = true; }
                if no_block { self.do_frame = true; }
                self.retry_at = Instant::now() + if watch { Duration::from_millis(50) } else { Duration::ZERO };
            } else {
                self.retry_at = Instant::now() + Duration::from_millis(50);
            }
        }
        if !self.pending_frame && (self.do_frame || (self.redraw && Instant::now() >= self.retry_at)) { self.window.request_redraw(); }
        Ok(false)
    }

    fn capture_root() -> PathBuf {
        std::env::var_os("WR_CAPTURE_PATH").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("../captures/wrench"))
    }

    fn key(&mut self, key: &Key) -> Result<(), String> {
        match key.as_ref() {
            Key::Named(NamedKey::ArrowLeft) => { self.thing.prev_frame(); self.do_frame = true; }
            Key::Named(NamedKey::ArrowRight) => { self.thing.next_frame(); self.do_frame = true; }
            Key::Character("l" | "L") => { self.looping = !self.looping; self.do_frame = self.looping; }
            Key::Character("y" | "Y") => {
                self.wrench.api.send_debug_cmd(DebugCommand::ClearCaches(ClearCache::all()));
                self.do_frame = true;
            }
            Key::Character("q" | "Q") => {
                self.gpu_timing = !self.gpu_timing;
                if !self.wrench.renderer.enable_gpu_profiling(self.gpu_timing) { println!("GPU timestamps are unavailable"); }
            }
            Key::Character("c" | "C") => {
                self.wrench.api.save_capture(Self::capture_root().join(format!("window-{}", self.number)), CaptureBits::all());
            }
            Key::Character("s" | "S") => {
                if self.screenshots.len() >= 4 || !self.wrench.renderer.has_presentable_output() { return Ok(()); }
                let root = Self::capture_root();
                std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
                let (handle, size) = self.wrench.renderer.record_frame(ImageFormat::RGBA8)?;
                self.screenshots.push(Screenshot { handle, size, pixels: vec![0; size.width as usize * size.height as usize * 4],
                    path: root.join(format!("window-{}.png", self.number)) });
                eprintln!("HAL WINDOW screenshot queued number={} size={}x{}", self.number, size.width, size.height);
            }
            Key::Character("x" | "X") => println!("Hit test: {:?}", self.wrench.api.hit_test(self.wrench.document_id, self.cursor).items),
            Key::Character("h" | "H") => help(),
            Key::Character("b" | "B" | "p" | "P" | "o" | "O" | "i" | "I" | "d" | "D" | "f" | "F" | "v" | "V" | "g" | "G" | "z" | "Z") => {
                println!("This GL debug overlay is unavailable in HAL");
            }
            _ => {}
        }
        self.needs_update = true;
        Ok(())
    }
}

impl Drop for Pane {
    fn drop(&mut self) { self.wrench.api.shut_down(true); }
}

fn help() { println!("Arrows: step; L: loop; N: new window; S: screenshot; C: capture; M: memory pressure; Y: clear caches; Q: GPU timing; X: hit test; Esc: close"); }

struct App<'a> {
    args: &'a clap::ArgMatches,
    options: &'a Options,
    dimensions: [u32; 2],
    initial_windows: usize,
    limit: Option<u64>,
    proxy: EventLoopProxy<Wake>,
    panes: HashMap<WindowId, Pane>,
    next_number: u64,
    started: bool,
    error: Option<String>,
}

impl App<'_> {
    fn open(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        if cfg!(target_os = "android") && !self.panes.is_empty() { return Err("Android supports one activity window".into()); }
        if self.panes.len() >= 16 { return Err("At most 16 HAL windows are supported".into()); }
        let number = self.next_number;
        self.next_number += 1;
        let window = Rc::new(event_loop.create_window(Window::default_attributes().with_title(format!("Wrench Vulkan {number}"))
            .with_inner_size(PhysicalSize::new(self.dimensions[0], self.dimensions[1]))).map_err(|error| error.to_string())?);
        let size = window.inner_size();
        let initial_size = DeviceIntSize::new(size.width as i32, size.height as i32);
        let size = DeviceIntSize::new(size.width.max(1) as i32, size.height.max(1) as i32);
        let scale = window.scale_factor() as f32;
        let notifier = Box::new(Notifier { window: window.id(), generation: number, proxy: self.proxy.clone() });
        let mut wrench = Wrench::new_hal_window(self.options, size, !self.args.is_present("no_subpixel_aa"), notifier,
            crate::hal::compositor_config(self.args)?, window.clone(), SurfaceOptions { vsync: self.args.is_present("vsync"), transparent: false })?;
        wrench.renderer.configure_filtering(crate::hal::filtering(self.args))?;
        wrench.rebuild_display_lists = self.args.is_present("rebuild");
        let show = self.args.subcommand_matches("show").unwrap();
        let path = Path::new(show.value_of("INPUT").unwrap());
        let mut thing = crate::hal::playback(&mut wrench, path, Some(show))?;
        thing.on_window_changed(size, scale);
        match crate::hal::compositor_clips_override(self.args)? {
            Some(enabled) => wrench.set_compositor_clips_override(enabled),
            None => wrench.set_compositor_clips_enabled(path.is_dir()),
        }
        if self.args.is_present("no_batch") {
            wrench.api.send_debug_cmd(DebugCommand::SetFlags(DebugFlags::DISABLE_BATCHING | DebugFlags::MISSING_SNAPSHOT_PINK));
        }
        let mut txn = Transaction::new();
        txn.set_document_view(DeviceIntRect::from_size(size));
        wrench.api.send_transaction(wrench.document_id, txn);
        thing.do_frame(&mut wrench);
        let (platform, xid) = crate::hal_platform::identity(&window)?;
        eprintln!("HAL WINDOW opened number={number} window={xid} size={}x{} scale={scale} platform={platform} adapter={}", size.width, size.height, wrench.renderer.info().name);
        window.request_redraw();
        self.panes.insert(window.id(), Pane { window, wrench, thing, number, size, scale,
            pending_size: Some(initial_size), pending_scale: None,
            occluded: false, minimized: false, suspended: false, needs_update: true, do_frame: false, pending_frame: true,
            redraw: false, looping: false, frames: 0, retry_at: Instant::now(), cursor: WorldPoint::zero(), screenshots: Vec::new(), gpu_timing: false });
        Ok(())
    }

    fn close(&mut self, id: WindowId, event_loop: &ActiveEventLoop) {
        if let Some(pane) = self.panes.remove(&id) { eprintln!("HAL WINDOW closed number={} frames={}", pane.number, pane.frames); }
        if self.panes.is_empty() { event_loop.exit(); }
    }

    fn fail(&mut self, error: String, event_loop: &ActiveEventLoop) { self.error = Some(error); event_loop.exit(); }
}

impl ApplicationHandler<Wake> for App<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.error.is_some() || event_loop.exiting() { return; }
        if !self.started {
            self.started = true;
            for _ in 0..self.initial_windows { if let Err(error) = self.open(event_loop) { self.fail(error, event_loop); break; } }
        } else {
            for pane in self.panes.values_mut() {
                pane.suspended = false;
                let size = pane.window.inner_size();
                pane.pending_size = Some(DeviceIntSize::new(size.width as i32, size.height as i32));
                pane.pending_scale = Some(pane.window.scale_factor() as f32);
                pane.needs_update = true;
                pane.window.request_redraw();
            }
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        if cfg!(target_os = "android") {
            let pending: usize = self.panes.values().map(|pane| pane.screenshots.len()).sum();
            self.panes.clear();
            self.started = false;
            eprintln!("HAL Android suspended; native surfaces retired, {pending} screenshots cancelled; playback restarts on resume");
            return;
        }
        for pane in self.panes.values_mut() {
            pane.suspended = true;
            pane.needs_update = true;
            if let Err(error) = pane.wrench.renderer.resize_surface([0, 0]) {
                self.error = Some(error);
                event_loop.exit();
                return;
            }
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, wake: Wake) {
        if let Some(pane) = self.panes.get_mut(&wake.window) {
            if pane.number != wake.generation { return; }
            pane.needs_update = true;
            pane.redraw |= wake.composite;
            if !pane.hidden() { pane.window.request_redraw(); }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(pane) = self.panes.get_mut(&id) else { return; };
        match event {
            WindowEvent::CloseRequested => self.close(id, event_loop),
            WindowEvent::RedrawRequested => { pane.redraw = true; pane.needs_update = true; }
            WindowEvent::Focused(focused) => {
                pane.redraw |= focused;
                pane.needs_update = true;
                if self.args.is_present("verbose") { eprintln!("HAL WINDOW focus number={} focused={focused}", pane.number); }
            }
            WindowEvent::Resized(size) => {
                pane.pending_size = Some(DeviceIntSize::new(size.width as i32, size.height as i32));
                pane.needs_update = true;
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                pane.pending_scale = Some(scale_factor as f32);
                pane.needs_update = true;
            }
            WindowEvent::Occluded(occluded) => { pane.occluded = occluded; pane.needs_update = true; }
            WindowEvent::CursorMoved { position, .. } => {
                pane.cursor = WorldPoint::new(position.x as f32 / pane.scale, position.y as f32 / pane.scale);
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.logical_key.as_ref() {
                    Key::Named(NamedKey::Escape) => self.close(id, event_loop),
                    Key::Character("n" | "N") => { if let Err(error) = self.open(event_loop) { self.fail(error, event_loop); } }
                    Key::Character("m" | "M") => {
                        for pane in self.panes.values_mut() {
                            pane.wrench.api.notify_memory_pressure();
                            pane.do_frame = true;
                            pane.needs_update = true;
                        }
                    }
                    _ => { if let Err(error) = pane.key(&event.logical_key) { self.fail(error, event_loop); } }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let watch = self.args.subcommand_matches("show").unwrap().is_present("watch");
        let no_block = self.args.is_present("no_block");
        let mut close = Vec::new();
        let mut retry = false;
        for (id, pane) in &mut self.panes {
            match pane.tick(self.limit, no_block, watch, self.args.is_present("verbose")) {
                Ok(true) => close.push(*id),
                Ok(false) => {}
                Err(error) => { self.error = Some(error); event_loop.exit(); return; }
            }
            retry |= !pane.screenshots.is_empty() || pane.wrench.renderer.memory_stats().in_flight > 0
                || (!pane.hidden() && (pane.redraw || pane.do_frame || pane.looping || watch));
        }
        for id in close { self.close(id, event_loop); }
        event_loop.set_control_flow(if no_block { ControlFlow::Poll } else if retry {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16))
        } else { ControlFlow::Wait });
    }

    fn exiting(&mut self, _: &ActiveEventLoop) { self.panes.clear(); }
}

pub fn run<'a>(args: &'a clap::ArgMatches, options: &'a Options, dimensions: [u32; 2]) -> Result<(), String> {
    if dimensions.contains(&0) || dimensions.iter().any(|size| *size > i32::MAX as u32) { return Err("Invalid window dimensions".into()); }
    for option in ["no_scissor", "color_target_init", "profiler_ui", "dump_shader_source", "slow_subpixel"] {
        if args.occurrences_of(option) > 0 { return Err(format!("--{option} is unavailable in HAL show")); }
    }
    let initial_windows: usize = args.value_of("hal_windows").unwrap_or("1").parse().map_err(|_| "Invalid window count")?;
    if !(1..=16).contains(&initial_windows) { return Err("HAL window count must be between 1 and 16".into()); }
    if cfg!(target_os = "android") && initial_windows != 1 { return Err("Android supports one activity window".into()); }
    let limit = args.value_of("hal_frames").map(|value| value.parse::<u64>().map_err(|_| "Invalid frame count")).transpose()?;
    if limit == Some(0) { return Err("HAL frame count must be positive".into()); }
    let event_loop = crate::hal_platform::event_loop::<Wake>()?;
    let mut app = App { args, options, dimensions, initial_windows, limit, proxy: event_loop.create_proxy(),
        panes: HashMap::new(), next_number: 1, started: false, error: None };
    help();
    event_loop.run_app(&mut app).map_err(|error| error.to_string())?;
    match app.error { Some(error) => Err(error), None => Ok(()) }
}
