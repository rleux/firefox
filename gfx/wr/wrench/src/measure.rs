/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use webrender::api::{DocumentId, FramePublishId, FrameReadyParams, RenderNotifier};
use webrender::api::units::{DeviceIntSize, FramebufferIntRect, FramebufferIntSize};
use crate::NotifierEvent;
use crate::reftest::ReftestRenderer;
use crate::wrench::{Wrench, WrenchThing};
use crate::yaml_frame_reader::YamlFrameReader;

pub struct Options {
    input: PathBuf,
    output: PathBuf,
    frames: usize,
    warmup: usize,
    inspection_ms: u64,
}

impl Options {
    pub fn from_args(args: &clap::ArgMatches) -> Result<Self, String> {
        if args.is_present("compositor") || args.value_of("hal_compositor").map_or(false, |mode| mode != "draw") {
            return Err("measure requires the Draw compositor".into());
        }
        if args.is_present("profiler_ui") {
            return Err("measure does not support the profiler overlay".into());
        }
        let args = args.subcommand_matches("measure").unwrap();
        let count = |name| -> Result<usize, String> {
            let value = args.value_of(name).unwrap().parse::<usize>()
                .map_err(|_| format!("Invalid {name}"))?;
            if value > 100_000 { return Err(format!("{name} exceeds 100000")); }
            Ok(value)
        };
        let frames = count("frames")?;
        if frames == 0 { return Err("frames must be positive".into()); }
        let inspection_ms = args.value_of("inspection_ms").unwrap().parse::<u64>()
            .map_err(|_| "Invalid inspection duration")?;
        if inspection_ms > 10_000 { return Err("inspection-ms exceeds 10000".into()); }
        Ok(Self {
            input: PathBuf::from(args.value_of("INPUT").unwrap()),
            output: PathBuf::from(args.value_of("OUTPUT").unwrap()),
            frames,
            warmup: count("warmup")?,
            inspection_ms,
        })
    }
}

struct Notifier(Sender<NotifierEvent>);

impl RenderNotifier for Notifier {
    fn clone(&self) -> Box<dyn RenderNotifier> { Box::new(Self(self.0.clone())) }
    fn wake_up(&self, _: bool) {}
    fn shut_down(&self) { let _ = self.0.send(NotifierEvent::ShutDown); }
    fn new_frame_ready(&self, _: DocumentId, _: FramePublishId, params: &FrameReadyParams) {
        let _ = self.0.send(NotifierEvent::WakeUp { composite_needed: params.render });
    }
}

pub fn notifier() -> (Box<dyn RenderNotifier>, Receiver<NotifierEvent>) {
    let (tx, rx) = channel();
    (Box::new(Notifier(tx)), rx)
}

pub fn run<R: ReftestRenderer>(
    wrench: &mut Wrench<R>,
    rx: &Receiver<NotifierEvent>,
    size: DeviceIntSize,
    options: &Options,
    backend: serde_json::Value,
    initialization: Duration,
) -> Result<(), String> {
    if size.width <= 0 || size.height <= 0 { return Err("Invalid target dimensions".into()); }
    let png = options.output.with_extension("png");
    if options.output == png || options.output.exists() || png.exists() {
        return Err("Measurement output or image already exists, or output has .png extension".into());
    }
    let mut output = OpenOptions::new().write(true).create_new(true).open(&options.output)
        .map_err(|e| e.to_string())?;
    let mut reader = YamlFrameReader::new(&options.input);
    wrench.configure_measurement();
    let mut samples = Vec::with_capacity(options.frames);
    let one_pixel = FramebufferIntRect::from_size(FramebufferIntSize::new(1, 1));
    for frame in 0..options.warmup + options.frames {
        if rx.try_recv().is_ok() { return Err("Unexpected extra frame notification".into()); }
        let start = Instant::now();
        reader.do_frame(wrench);
        let render_requested = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(NotifierEvent::WakeUp { composite_needed }) => composite_needed,
            Ok(NotifierEvent::ShutDown) => return Err("Renderer shut down during measurement".into()),
            Err(e) => return Err(format!("Frame notification failed: {e}")),
        };
        let ready = start.elapsed();
        R::render_test(wrench);
        let submitted = start.elapsed();
        let pixel = wrench.renderer.read_test_pixels(one_pixel);
        let completed = start.elapsed();
        if pixel.len() != 4 { return Err("Completion readback failed".into()); }
        if frame >= options.warmup {
            samples.push(serde_json::json!({
                "sceneReadyNs": ready.as_nanos() as u64,
                "renderCallNs": (submitted - ready).as_nanos() as u64,
                "completionReadbackNs": (completed - submitted).as_nanos() as u64,
                "totalNs": completed.as_nanos() as u64,
                "renderRequested": render_requested,
            }));
        }
    }
    let rect = FramebufferIntRect::from_size(FramebufferIntSize::new(size.width, size.height));
    let pixels = wrench.renderer.read_test_pixels(rect);
    let mut image = image::RgbaImage::from_raw(size.width as u32, size.height as u32, pixels)
        .ok_or("Final readback dimensions mismatch")?;
    image = image::imageops::flip_vertical(&image);
    image.save(&png).map_err(|e| e.to_string())?;
    serde_json::to_writer_pretty(&mut output, &serde_json::json!({
        "schemaVersion": 1,
        "completed": true,
        "backend": backend,
        "size": [size.width, size.height],
        "input": options.input,
        "image": png,
        "frames": options.frames,
        "warmupFrames": options.warmup,
        "inspectionMs": options.inspection_ms,
        "contextAndRendererInitializationNs": initialization.as_nanos() as u64,
        "completion": "one-pixel synchronous readback per frame",
        "presents": 0,
        "samples": samples,
    })).map_err(|e| e.to_string())?;
    output.flush().map_err(|e| e.to_string())?;
    if options.inspection_ms != 0 {
        std::thread::sleep(Duration::from_millis(options.inspection_ms));
    }
    Ok(())
}
