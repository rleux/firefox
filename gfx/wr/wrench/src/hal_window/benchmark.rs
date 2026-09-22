/* This Source Code Form is subject to the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{fs::OpenOptions, path::PathBuf, time::Instant};

pub(super) struct Benchmark {
    pub early: bool,
    output: PathBuf,
    warmup: u64,
    previous: Option<Instant>,
    samples: Vec<serde_json::Value>,
}

impl Benchmark {
    pub fn new(limit: Option<u64>, windows: usize) -> Result<Option<Self>, String> {
        let Some(output) = std::env::var_os("WR_WRENCH_ACQUIRE_BENCH") else {
            if std::env::var_os("WR_WRENCH_ACQUIRE_EARLY").is_some() {
                return Err("Acquisition control requires WR_WRENCH_ACQUIRE_BENCH".into());
            }
            return Ok(None);
        };
        let output = PathBuf::from(output);
        if output.exists() || output.with_extension("png").exists() {
            return Err("Window benchmark output already exists".into());
        }
        let early = match std::env::var("WR_WRENCH_ACQUIRE_EARLY").as_deref() {
            Ok("1") => true,
            Ok("0") | Err(_) => false,
            _ => return Err("Invalid WR_WRENCH_ACQUIRE_EARLY".into()),
        };
        let warmup = std::env::var("WR_WRENCH_BENCH_WARMUP").unwrap_or_else(|_| "30".into())
            .parse::<u64>().map_err(|_| "Invalid window benchmark warmup")?;
        if windows != 1 || limit.map_or(true, |limit| limit <= warmup || limit > 100_000) {
            return Err("Window benchmark requires one window and a bounded frame count above warmup".into());
        }
        Ok(Some(Self { early, output, warmup, previous: None, samples: Vec::new() }))
    }

    pub fn record(&mut self, frame: u64, render_ns: u128, acquire_ns: u128, present_ns: u128) {
        let now = Instant::now();
        let interval = self.previous.replace(now).map(|previous| now.duration_since(previous).as_nanos());
        if frame > self.warmup {
            self.samples.push(serde_json::json!({
                "renderNs": render_ns as u64, "acquireNs": acquire_ns as u64,
                "presentNs": present_ns as u64, "intervalNs": interval.map(|ns| ns as u64),
            }));
        }
    }

    pub fn finish(&self, pane: &super::Pane) -> Result<(), String> {
        use webrender::api::units::{FramebufferIntRect, FramebufferIntSize};
        let rect = FramebufferIntRect::from_size(FramebufferIntSize::new(pane.size.width, pane.size.height));
        let pixels = pane.wrench.renderer.read_pixels_rgba8(rect)?;
        let png = self.output.with_extension("png");
        crate::png::save(&png, pixels, pane.size,
            crate::png::SaveSettings { flip_vertical: true, try_crop: false });
        let surface = pane.wrench.renderer.surface_info().ok_or("Missing benchmark surface")?;
        let output = OpenOptions::new().write(true).create_new(true).open(&self.output)
            .map_err(|error| error.to_string())?;
        serde_json::to_writer_pretty(output, &serde_json::json!({
            "completed": true, "earlyAcquire": self.early, "warmupFrames": self.warmup,
            "frames": pane.frames, "samples": self.samples, "image": png,
            "size": [pane.size.width, pane.size.height], "adapter": pane.wrench.renderer.info().name,
            "backend": format!("{:?}", pane.wrench.renderer.info().backend),
            "surface": format!("{surface:?}"),
            "scope": "Wrench acquire-before-render versus acquire-after-render; resource update precedes both",
        })).map_err(|error| error.to_string())
    }
}
