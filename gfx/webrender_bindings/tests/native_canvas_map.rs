/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::{
    native_canvas::{self, DrawContext, BACKGROUND, PIXELS},
    unified,
};
use std::{
    rc::Rc,
    sync::{mpsc, Arc, Weak},
    thread::JoinHandle,
    time::Duration,
};
use webrender::api::{ExternalTextureHandle, ImageFormat};
use webrender::vulkan::{Device, SharedTimeline, Submission};
use webrender_bindings::vulkan_native::WrVulkanNativePublication;
use wgpu_bindings::unified::ProcessOwner;
use wgpu_core_remote::id::*;
use wgpu_hal::shared_image::ImageLease;
use wgpu_types as wgt;

const LATEST: [[u8; 4]; 6] = [
    [201, 37, 11, 255],
    [5, 99, 13, 128],
    [53, 7, 31, 64],
    [83, 137, 11, 192],
    [101, 23, 67, 255],
    [31, 11, 19, 64],
];

struct Armed(bool);
impl Drop for Armed {
    fn drop(&mut self) {
        if self.0 {
            std::process::exit(88);
        }
    }
}
struct Gate {
    release: mpsc::Sender<()>,
    controller: Option<JoinHandle<bool>>,
    blocker: Submission,
    _timeline: Rc<SharedTimeline>,
    _device: Rc<Device>,
}
impl Gate {
    fn new(owner: &Arc<ProcessOwner>) -> Self {
        let control = unified::owner();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let controller = std::thread::spawn(move || {
            let device = unified::graphics(&control);
            let timeline = SharedTimeline::new(&device).unwrap();
            let mut signal = Submission::new(&device).unwrap();
            signal.recording().unwrap().signal_timeline(&timeline, 1).unwrap();
            let mut armed = Armed(true);
            ready_tx.send(timeline.export().unwrap()).unwrap();
            let explicit = released.recv_timeout(Duration::from_secs(5)).is_ok();
            if signal.submit().is_err() {
                std::process::exit(88);
            }
            if !matches!(signal.wait(Some(Duration::from_secs(30))), Ok(true)) {
                std::process::exit(88);
            }
            drop((signal, timeline, device));
            control.begin_shutdown();
            if control.context().drain().is_err() {
                std::process::exit(88);
            }
            armed.0 = false;
            explicit
        });
        let handle = ready_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let device = unified::graphics(owner);
        let timeline = SharedTimeline::import(&device, &handle).unwrap();
        let mut blocker = Submission::new(&device).unwrap();
        blocker.recording().unwrap().wait_timeline(&timeline, 1).unwrap();
        if blocker.submit().is_err() {
            std::process::exit(88);
        }
        Self {
            release,
            controller: Some(controller),
            blocker,
            _timeline: timeline,
            _device: device,
        }
    }
    fn release(&mut self) -> bool {
        let _ = self.release.send(());
        let explicit = match self.controller.take().unwrap().join() {
            Ok(explicit) => explicit,
            Err(_) => std::process::exit(88),
        };
        if !matches!(self.blocker.wait(Some(Duration::from_secs(30))), Ok(true)) {
            std::process::exit(88);
        }
        explicit
    }
}

pub struct Fixture {
    owner: Arc<ProcessOwner>,
    draw: Option<DrawContext>,
    weak: [Weak<dyn ImageLease>; 2],
    gate: Option<Gate>,
}

#[no_mangle]
pub extern "C" fn wr_test_native_canvas_map_new(
    first: &mut *mut WrVulkanNativePublication,
    latest: &mut *mut WrVulkanNativePublication,
) -> *mut Fixture {
    let owner = unified::owner();
    let global = unified::actor(&owner, false);
    let (a, a_index, a_weak) =
        native_canvas::source(&owner, &global, wgt::TextureFormat::Rgba8Unorm, true, true, &PIXELS);
    let a = unsafe { WrVulkanNativePublication::new(owner.clone(), a, wgt::TextureFormat::Rgba8UnormSrgb) }.unwrap();
    let (b, b_index, b_weak) =
        native_canvas::source(&owner, &global, wgt::TextureFormat::Bgra8Unorm, true, true, &LATEST);
    let b = unsafe { WrVulkanNativePublication::new(owner.clone(), b, wgt::TextureFormat::Bgra8UnormSrgb) }.unwrap();
    native_canvas::retire(&global, a_index);
    native_canvas::retire(&global, b_index);
    let device = global.resolve_device_id(DeviceId::zip(1, 1));
    assert_eq!(device.get_hal_counters().textures.read(), 0);
    device.destroy();
    drop((device, global));
    let draw = DrawContext::new(&owner);
    *first = Box::into_raw(Box::new(a));
    *latest = Box::into_raw(Box::new(b));
    Box::into_raw(Box::new(Fixture {
        owner,
        draw: Some(draw),
        weak: [a_weak, b_weak],
        gate: None,
    }))
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_map_renderer(fixture: &mut Fixture) -> &mut webrender::Renderer {
    fixture.draw.as_mut().unwrap().renderer.as_mut().unwrap()
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_map_sample(fixture: &mut Fixture, normal: u64, opaque: u64) {
    let draw = fixture.draw.as_mut().unwrap();
    draw.render(
        [ExternalTextureHandle(normal), ExternalTextureHandle(opaque)],
        ImageFormat::RGBA8,
    );
    let bytes = draw.pixels();
    assert_eq!(bytes.len(), 48);
    for row in 0..2 {
        for panel in 0..2 {
            for column in 0..3 {
                let pixel = PIXELS[row * 3 + column];
                let offset = (row * 6 + panel * 3 + column) * 4;
                for channel in 0..3 {
                    let source = native_canvas::linear(pixel[channel]) * 255.;
                    let blend = if panel == 0 {
                        BACKGROUND[channel] as f64 * (255 - pixel[3]) as f64 / 255.
                    } else {
                        0.
                    };
                    assert!((bytes[offset + channel] as i16 - (source + blend).round() as i16).abs() <= 1);
                }
                assert_eq!(bytes[offset + 3], 255);
            }
        }
    }
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_map_gate_begin(fixture: &mut Fixture) {
    assert!(fixture.gate.is_none());
    fixture.gate = Some(Gate::new(&fixture.owner));
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_map_gate_release(fixture: &mut Fixture) -> bool {
    match fixture.gate.take() {
        Some(mut gate) => gate.release(),
        None => true,
    }
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_map_texture_count(fixture: &Fixture) -> isize {
    fixture.owner.graphics().device.get_internal_counters().textures.read()
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_map_live(fixture: &Fixture, latest: bool) -> bool {
    fixture.weak[usize::from(latest)].upgrade().is_some()
}
#[no_mangle]
pub extern "C" fn wr_test_native_canvas_map_finish(fixture: &mut Fixture) {
    assert!(fixture.gate.is_none());
    fixture.draw.take().unwrap().finish();
    assert!(fixture.weak.iter().all(|weak| weak.upgrade().is_none()));
}
#[no_mangle]
pub unsafe extern "C" fn wr_test_native_canvas_map_delete(fixture: *mut Fixture) {
    let mut fixture = Box::from_raw(fixture);
    let explicit = wr_test_native_canvas_map_gate_release(&mut fixture);
    if let Some(draw) = fixture.draw.take() {
        draw.finish();
    }
    fixture.owner.begin_shutdown();
    if fixture.owner.context().drain().is_err() {
        std::process::exit(88);
    }
    assert!(explicit, "Native map gate required fallback release");
}
