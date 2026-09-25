/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock, atomic::{AtomicU64, Ordering}};
use std::time::{Duration, Instant};

macro_rules! fields {
    ($kind:ident, $($field:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy, Debug)]
        pub enum $kind { $($field),+ }
        impl $kind {
            pub const ALL: &'static [Self] = &[$(Self::$field),+];
            pub fn name(self) -> &'static str {
                match self { $(Self::$field => $name),+ }
            }
        }
    };
}

fields!(RenderCounter,
    FrameReady => "frameReady",
    RenderRequested => "renderRequested",
    NoRenderRequested => "noRenderRequested",
    ScrolledRequests => "scrolledRequests",
    ForceRedraws => "forceRedraws",
    WakeRender => "wakeRender",
    WakeUpdate => "wakeUpdate",
    Updates => "updates",
    Executions => "executions",
    OffscreenExecutions => "offscreenExecutions",
    RasterizedTiles => "rasterizedTiles",
    FullCompositions => "fullCompositions",
    PartialCompositions => "partialCompositions",
    ComposedPixels => "composedPixels",
    Acquires => "acquires",
    Presents => "presents",
    FullPresentUpdates => "fullPresentUpdates",
    PartialPresentUpdates => "partialPresentUpdates",
    UnchangedPresentUpdates => "unchangedPresentUpdates",
    PresentPixels => "presentPixels",
    Discards => "discards",
    QueueSubmissions => "queueSubmissions",
    SurfaceSubmissions => "surfaceSubmissions",
    CompletedSubmissions => "completedSubmissions",
    Polls => "polls",
    ExternalLeaseAcquires => "externalLeaseAcquires",
    ExternalLeaseReleases => "externalLeaseReleases",
    ResourceUploads => "resourceUploads",
    ResourceUploadBytes => "resourceUploadBytes",
    Readbacks => "readbacks",
    ReusedOutputs => "reusedOutputs",
    HiddenSkips => "hiddenSkips",
);

fields!(RenderGauge,
    PendingSubmissions => "pendingSubmissions",
    ExternalLeases => "externalLeases",
    RetainedOutputBytes => "retainedOutputBytes",
    InitializedSurfaceImages => "initializedSurfaceImages",
    TextureBytes => "textureBytes",
    BufferBytes => "bufferBytes",
);

const COUNTERS: usize = RenderCounter::ALL.len();
const GAUGES: usize = RenderGauge::ALL.len();
const FRAME_TABLES: [&str; 6] = [
    "sPrimitiveHeadersF",
    "sPrimitiveHeadersI",
    "sGpuBufferF",
    "sGpuBufferI",
    "sTransformPalette",
    "sRenderTasks",
];

#[derive(Default)]
struct FrameTableMetrics {
    source_bytes: AtomicU64,
    upload_bytes: AtomicU64,
    updated_bytes: AtomicU64,
    pack_ns: AtomicU64,
    buffer_uploads: AtomicU64,
    texture_uploads: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct RenderMetricsSnapshot {
    pub device_id: u64,
    pub renderer_id: u64,
    pub monotonic_ns: u64,
    counters: [u64; COUNTERS],
    gauges: [u64; GAUGES],
    peaks: [u64; GAUGES],
    last_work_ns: [u64; COUNTERS],
    frame_tables: [[u64; 6]; 6],
}

impl RenderMetricsSnapshot {
    pub fn count(&self, counter: RenderCounter) -> u64 { self.counters[counter as usize] }
    pub fn gauge(&self, gauge: RenderGauge) -> u64 { self.gauges[gauge as usize] }

    fn json(&self, sequence: u64, final_report: bool) -> String {
        let counters = RenderCounter::ALL.iter().map(|&counter| {
            format!("\"{}\":{}", counter.name(), self.count(counter))
        }).collect::<Vec<_>>().join(",");
        let gauges = RenderGauge::ALL.iter().map(|&gauge| {
            format!("\"{}\":{}", gauge.name(), self.gauge(gauge))
        }).collect::<Vec<_>>().join(",");
        let peaks = RenderGauge::ALL.iter().map(|&gauge| {
            format!("\"{}\":{}", gauge.name(), self.peaks[gauge as usize])
        }).collect::<Vec<_>>().join(",");
        let last_work = RenderCounter::ALL.iter().map(|&counter| {
            format!("\"{}\":{}", counter.name(), self.last_work_ns[counter as usize])
        }).collect::<Vec<_>>().join(",");
        let frame_tables = FRAME_TABLES.iter().enumerate().map(|(index, name)| {
            let values = self.frame_tables[index];
            format!("\"{name}\":{{\"sourceBytes\":{},\"writeBytes\":{},\"updatedBytes\":{},\"prepareNs\":{},\"bufferUploads\":{},\"textureUploads\":{}}}",
                values[0], values[1], values[2], values[3], values[4], values[5])
        }).collect::<Vec<_>>().join(",");
        format!("{{\"version\":1,\"pid\":{},\"deviceId\":{},\"rendererId\":{},\"sequence\":{},\"monotonicNs\":{},\"final\":{},\"counters\":{{{}}},\"gauges\":{{{}}},\"peaks\":{{{}}},\"lastWorkNs\":{{{}}},\"frameTables\":{{{}}}}}",
            std::process::id(), self.device_id, self.renderer_id, sequence,
            self.monotonic_ns, final_report, counters, gauges, peaks, last_work, frame_tables)
    }
}

pub struct RenderMetrics {
    device_id: u64,
    renderer_id: u64,
    start: Instant,
    last_report: Mutex<Instant>,
    sequence: AtomicU64,
    counters: [AtomicU64; COUNTERS],
    gauges: [AtomicU64; GAUGES],
    peaks: [AtomicU64; GAUGES],
    last_work_ns: [AtomicU64; COUNTERS],
    frame_tables: [FrameTableMetrics; 6],
}

impl RenderMetrics {
    #[cfg(test)]
    pub(crate) fn for_test(device_id: u64, renderer_id: u64) -> Arc<Self> {
        Arc::new(Self::create(device_id, renderer_id))
    }

    pub(crate) fn new(device_id: u64, renderer: bool) -> Option<Arc<Self>> {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        if !*ENABLED.get_or_init(|| std::env::var("WR_HAL_RENDER_METRICS").as_deref() == Ok("1")) {
            return None;
        }
        static NEXT_RENDERER: AtomicU64 = AtomicU64::new(1);
        let renderer_id = if renderer { NEXT_RENDERER.fetch_add(1, Ordering::Relaxed) } else { 0 };
        Some(Arc::new(Self::create(device_id, renderer_id)))
    }

    fn create(device_id: u64, renderer_id: u64) -> Self {
        static START: OnceLock<Instant> = OnceLock::new();
        let start = *START.get_or_init(Instant::now);
        Self {
            device_id, renderer_id, start, last_report: Mutex::new(Instant::now()),
            sequence: AtomicU64::new(0),
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
            gauges: std::array::from_fn(|_| AtomicU64::new(0)),
            peaks: std::array::from_fn(|_| AtomicU64::new(0)),
            last_work_ns: std::array::from_fn(|_| AtomicU64::new(0)),
            frame_tables: std::array::from_fn(|_| FrameTableMetrics::default()),
        }
    }

    fn now(&self) -> u64 { self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64 }

    pub(crate) fn add(&self, counter: RenderCounter, count: u64) {
        if count == 0 { return; }
        self.counters[counter as usize].fetch_add(count, Ordering::Relaxed);
        self.last_work_ns[counter as usize].fetch_max(self.now(), Ordering::Relaxed);
    }

    pub(crate) fn set(&self, gauge: RenderGauge, value: u64) {
        self.gauges[gauge as usize].store(value, Ordering::Relaxed);
        self.peaks[gauge as usize].fetch_max(value, Ordering::Relaxed);
    }

    pub(crate) fn retain(&self, gauge: RenderGauge) {
        let value = self.gauges[gauge as usize].fetch_add(1, Ordering::Relaxed) + 1;
        self.peaks[gauge as usize].fetch_max(value, Ordering::Relaxed);
    }

    pub(crate) fn release(&self, gauge: RenderGauge) {
        self.gauges[gauge as usize].fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn record_frame_table(
        &self,
        name: &str,
        source_bytes: u64,
        write_bytes: u64,
        updated_bytes: u64,
        prepare_ns: u64,
        storage_buffer: bool,
    ) {
        let Some(index) = FRAME_TABLES.iter().position(|&table| table == name) else {
            return;
        };
        let table = &self.frame_tables[index];
        table.source_bytes.fetch_add(source_bytes, Ordering::Relaxed);
        table.upload_bytes.fetch_add(write_bytes, Ordering::Relaxed);
        table.updated_bytes.fetch_add(updated_bytes, Ordering::Relaxed);
        table.pack_ns.fetch_add(prepare_ns, Ordering::Relaxed);
        let uploads = if storage_buffer { &table.buffer_uploads } else { &table.texture_uploads };
        uploads.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RenderMetricsSnapshot {
        let counters = std::array::from_fn(|i| self.counters[i].load(Ordering::Relaxed));
        let gauges = std::array::from_fn(|i| self.gauges[i].load(Ordering::Relaxed));
        let peaks = std::array::from_fn(|i| self.peaks[i].load(Ordering::Relaxed));
        let last_work_ns = std::array::from_fn(|i| self.last_work_ns[i].load(Ordering::Relaxed));
        let frame_tables = std::array::from_fn(|i| [
            self.frame_tables[i].source_bytes.load(Ordering::Relaxed),
            self.frame_tables[i].upload_bytes.load(Ordering::Relaxed),
            self.frame_tables[i].updated_bytes.load(Ordering::Relaxed),
            self.frame_tables[i].pack_ns.load(Ordering::Relaxed),
            self.frame_tables[i].buffer_uploads.load(Ordering::Relaxed),
            self.frame_tables[i].texture_uploads.load(Ordering::Relaxed),
        ]);
        RenderMetricsSnapshot {
            device_id: self.device_id,
            renderer_id: self.renderer_id,
            monotonic_ns: self.now(),
            counters, gauges, peaks, last_work_ns, frame_tables,
        }
    }

    pub(crate) fn report_if_due(&self) {
        let Ok(mut previous) = self.last_report.try_lock() else { return; };
        if previous.elapsed() >= Duration::from_secs(1) {
            *previous = Instant::now();
            self.report(false);
        }
    }

    fn report(&self, final_report: bool) {
        self.write_report(&mut std::io::stderr().lock(), final_report);
    }

    fn write_report(&self, output: &mut impl Write, final_report: bool) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let line = format!("WR HAL render metrics: {}\n", self.snapshot().json(sequence, final_report));
        let _ = output.write_all(line.as_bytes());
    }
}

impl Drop for RenderMetrics {
    fn drop(&mut self) { self.report(true); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_sink_failure_does_not_interrupt_shutdown() {
        struct BrokenPipe;
        impl Write for BrokenPipe {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let metrics = RenderMetrics::create(1, 2);
        metrics.write_report(&mut BrokenPipe, true);
        assert_eq!(metrics.sequence.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn concurrent_notifications_and_completion_keep_monotonic_totals() {
        let metrics = Arc::new(RenderMetrics::create(9, 7));
        let producer = metrics.clone();
        let thread = std::thread::spawn(move || {
            for _ in 0..1000 {
                producer.add(RenderCounter::FrameReady, 1);
                producer.retain(RenderGauge::ExternalLeases);
            }
        });
        for _ in 0..1000 {
            metrics.add(RenderCounter::Polls, 1);
            let sample = metrics.snapshot();
            assert!(sample.last_work_ns.iter().all(|&work| work <= sample.monotonic_ns));
        }
        thread.join().unwrap();
        let before = metrics.snapshot();
        for _ in 0..1000 { metrics.release(RenderGauge::ExternalLeases); }
        let after = metrics.snapshot();
        assert_eq!(before.count(RenderCounter::FrameReady), 1000);
        assert_eq!(after.count(RenderCounter::Polls), 1000);
        assert_eq!(before.gauge(RenderGauge::ExternalLeases), 1000);
        assert_eq!(after.gauge(RenderGauge::ExternalLeases), 0);
        assert_eq!(after.peaks[RenderGauge::ExternalLeases as usize], 1000);
        assert!(after.monotonic_ns >= before.monotonic_ns);
        assert!(after.json(1, false).contains("\"rendererId\":7"));
    }
}
