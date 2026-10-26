/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;
use crate::device::wgpu::{Options, SharedDeviceOptions, SurfaceOptions};
use crate::device::wgpu::window_surface::tests::x11::{X11Display, X11Window};
use crate::device::wgpu::tests::{upload_queue, validation_logging, ERRORS};
use raw_window_handle::{DisplayHandle, WindowHandle, HasDisplayHandle, HasWindowHandle, HandleError};
use std::{
    cell::{Cell, RefCell},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

struct CheckedWindow {
    window: X11Window,
    thread: std::thread::ThreadId,
    lifetime: Rc<RefCell<Option<hal::renderer::SurfaceLifetime>>>,
    released: Rc<Cell<bool>>,
}

impl HasDisplayHandle for CheckedWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        self.window.display_handle()
    }
}

impl HasWindowHandle for CheckedWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.window.window_handle()
    }
}

impl Drop for CheckedWindow {
    fn drop(&mut self) {
        assert_eq!(self.thread, std::thread::current().id());
        if let Some(lifetime) = self.lifetime.borrow().as_ref() {
            assert!(lifetime.is_destroyed());
        }
        self.released.set(true);
    }
}

unsafe extern "C" fn fatal_process() -> ! {
    std::process::exit(86)
}

fn setup() -> (Rc<Device>, Swapchain, Rc<Cell<bool>>) {
    validation_logging();
    let display = unsafe {
        let xlib = libloading::Library::new("libX11.so.6").unwrap();
        assert_ne!(
            xlib.get::<unsafe extern "C" fn() -> i32>(b"XInitThreads\0")
                .unwrap()(),
            0
        );
        Rc::new(X11Display::new())
    };
    let released = Rc::new(Cell::new(false));
    let lifetime = Rc::new(RefCell::new(None));
    let window = Rc::new(CheckedWindow {
        window: unsafe { X11Window::with_display(display.clone()) },
        thread: std::thread::current().id(),
        lifetime: lifetime.clone(),
        released: released.clone(),
    });
    let (_, instance) = unsafe {
        hal::shared::create_presentation_instance(&hal::InstanceDescriptor {
            name: "WR shared surface owner tests",
            flags: wgt::InstanceFlags::DEBUG | wgt::InstanceFlags::VALIDATION,
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            telemetry: None,
            display: None,
        })
    }
    .unwrap();
    let surface = unsafe {
        instance.create_surface(
            window.display_handle().unwrap().as_raw(),
            window.window_handle().unwrap().as_raw(),
        )
    }
    .unwrap();
    let exposed = unsafe { instance.enumerate_adapters(None) }
        .into_iter()
        .find(|adapter| {
            adapter.info.device_type != wgt::DeviceType::Cpu
                && unsafe {
                    adapter
                        .adapter
                        .surface_capabilities(surface.as_ref())
                        .is_some()
                }
        })
        .unwrap();
    drop(surface);
    let pair = unsafe {
        hal::shared::open_shared_device(
            &*exposed.adapter,
            Device::shared_features(exposed.features),
            &exposed.capabilities.limits,
            &wgt::MemoryHints::MemoryUsage,
        )
    }
    .unwrap();
    let context = pair.context.clone();
    let owner = Rc::new(
        unsafe {
            Device::from_shared_endpoint_with_surface(
                Arc::new(pair.graphics),
                instance,
                exposed,
                SharedDeviceOptions {
                    context,
                    recover: Rc::new(|_| panic!("Unexpected recovery")),
                    fatal_process,
                },
                &Options {
                    window: Some(window),
                    display_owner: Some(display),
                    ..Default::default()
                },
            )
        }
        .unwrap(),
    );
    drop(pair.compute);
    assert!(hal::renderer::graphics_capabilities(
        owner.instance.as_ref(), owner.adapter.as_ref(), owner.open.device.as_ref(),
    ).unwrap().validation_enabled);
    let queue = Rc::new(upload_queue(&owner));
    let chain = Swapchain::new(&queue).unwrap();
    *lifetime.borrow_mut() = Some(
        hal::renderer::surface_lifetime(chain.surface.as_ref().unwrap().raw.as_ref()).unwrap(),
    );
    (owner, chain, released)
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn retained_surface_target_releases_image_before_window_owner() {
    let (owner, mut chain, released) = setup();
    assert!(chain.prepare_target([64, 48]).unwrap());
    let target = chain.pass_target().unwrap();
    // Close with an old image/view retained solely for later destruction.
    chain.acquired.take();
    chain.target.take();
    chain.set_window(None, SurfaceOptions::default()).unwrap();
    drop(chain);
    assert!(!released.get());
    drop(target);
    assert!(released.get());
    let logical = unsafe {
        owner
            .shared
            .as_ref()
            .unwrap()
            .options
            .context
            .clone()
            .open_compute_device(
                owner.adapter.as_ref(),
                wgt::Features::empty(),
                &owner.capabilities.limits,
                &wgt::MemoryHints::MemoryUsage,
            )
    }
    .unwrap();
    drop(logical);
    drop(owner);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

fn delayed_backend_view(fatal: bool) {
    let (owner, mut chain, released) = setup();
    assert!(chain.prepare_target([64, 48]).unwrap());
    let view = unsafe {
        owner.open.device.create_texture_view(
            chain.acquired.as_ref().unwrap().texture.as_ref().borrow(),
            &hal::TextureViewDescriptor {
                label: None,
                swizzle: Default::default(),
                format: chain.config.as_ref().unwrap().format,
                dimension: wgt::TextureViewDimension::D2,
                usage: wgt::TextureUses::COLOR_TARGET,
                range: wgt::ImageSubresourceRange {
                    mip_level_count: Some(1),
                    array_layer_count: Some(1),
                    ..Default::default()
                },
            },
        )
    }
    .unwrap();
    chain.discard_acquired().unwrap();
    let surface_owner = Rc::get_mut(&mut chain.surface.as_mut().unwrap().owner).unwrap();
    let waiting = surface_owner.waiting.clone();
    if fatal {
        surface_owner.timeout = Duration::from_millis(50);
        std::mem::forget(view);
        drop(chain);
        panic!("Owner wait returned without native destruction");
    }
    let native = owner.open.clone();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !waiting.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "Renderer did not wait for backend owners"
            );
            std::thread::yield_now();
        }
        unsafe { native.device.destroy_texture_view(view) };
    });
    drop(chain);
    worker.join().unwrap();
    assert!(released.get());
    drop(owner);
    assert_eq!(ERRORS.load(Ordering::Relaxed), 0);
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn window_owner_waits_for_backend_view_on_another_thread() {
    delayed_backend_view(false);
}

#[test]
#[ignore = "Requires 24-bit Xvfb, shared Vulkan maintenance1 and Khronos validation"]
fn unretired_surface_owner_terminates_before_window_release() {
    const CHILD: &str = "WR_UNRETIRED_SURFACE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        delayed_backend_view(true);
        unreachable!();
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "device::wgpu::swapchain::target::shared_surface_tests::unretired_surface_owner_terminates_before_window_release",
               "--ignored", "--nocapture"])
        .env(CHILD, "1").spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(86));
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Unretired surface child exceeded bounded recovery");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
