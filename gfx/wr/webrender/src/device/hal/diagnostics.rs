/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Instant;

fn flag(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

pub fn quiet() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| flag("WR_WEBGPU_BENCHMARK_QUIET") || flag("WR_WEBGL_BENCHMARK_QUIET"))
}

pub fn force_dmabuf_copy() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| flag("WR_WEBGPU_FORCE_DMABUF_COPY"))
}

pub fn force_webgl_sync() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| flag("WR_WEBGL_FORCE_SYNC"))
}

fn enabled() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| flag("WR_WEBGPU_SYNC_INSTRUMENTATION") || flag("WR_WEBGL_SYNC_INSTRUMENTATION"))
}

#[derive(Default)]
struct Sample {
    count: u64,
    total: u64,
    max: u64,
    buckets: [u64; 9],
}

impl Sample {
    fn print(&self, thread: &str, event: &str) {
        let source = if flag("WR_WEBGL_SYNC_INSTRUMENTATION") { "WebGL" } else { "WebGPU" };
        eprintln!(
            "{source} DMA-BUF sync metrics: {{\"pid\":{},\"thread\":\"{}\",\"event\":\"{}\",\"count\":{},\"totalNs\":{},\"maxNs\":{},\"buckets\":{:?}}}",
            std::process::id(), thread, event, self.count, self.total, self.max, self.buckets,
        );
    }
}

struct Metrics {
    thread: String,
    samples: BTreeMap<&'static str, Sample>,
}

impl Drop for Metrics {
    fn drop(&mut self) {
        for (event, sample) in &self.samples {
            sample.print(&self.thread, event);
        }
    }
}

thread_local! {
    static METRICS: RefCell<Metrics> = RefCell::new(Metrics {
        thread: format!("{:?}", std::thread::current().id()),
        samples: BTreeMap::new(),
    });
    static TRANSPORTS: Cell<u8> = const { Cell::new(0) };
}

pub fn transport(direct: bool) {
    TRANSPORTS.with(|seen| {
        let bit = if direct { 1 } else { 2 };
        if seen.get() & bit == 0 {
            seen.set(seen.get() | bit);
            eprintln!(
                "WebRender Vulkan DMA-BUF selected transport: {}",
                if direct { "direct" } else { "copy" });
        }
    });
}

pub fn webgl_transport() {
    TRANSPORTS.with(|seen| {
        if seen.get() & 4 == 0 {
            seen.set(seen.get() | 4);
            eprintln!("WebRender Vulkan WebGL selected transport: direct; synchronization: {}",
                if force_webgl_sync() { "sync" } else { "async" });
        }
    });
}

pub struct Span {
    event: &'static str,
    start: Option<Instant>,
}

impl Span {
    pub fn new(event: &'static str) -> Self {
        Self {
            event,
            start: enabled().then(Instant::now),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        let Some(start) = self.start else {
            return;
        };
        let elapsed = start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        METRICS.with(|metrics| {
            let mut metrics = metrics.borrow_mut();
            let sample = metrics.samples.entry(self.event).or_default();
            sample.count += 1;
            sample.total += elapsed;
            sample.max = sample.max.max(elapsed);
            let bounds = [
                10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000, 10_000_000, 50_000_000,
            ];
            let bucket = bounds
                .iter()
                .position(|bound| elapsed <= *bound)
                .unwrap_or(8);
            sample.buckets[bucket] += 1;
            if sample.count % 128 == 0 {
                metrics.samples[self.event].print(&metrics.thread, self.event);
            }
        });
    }
}
