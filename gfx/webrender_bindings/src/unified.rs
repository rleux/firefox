/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

pub struct WrUnifiedOwner {
    #[cfg(all(
        feature = "vulkan",
        any(target_os = "linux", target_os = "windows", target_os = "android")
    ))]
    owner: std::sync::Arc<wgpu_bindings::unified::ProcessOwner>,
}
#[cfg(all(
    feature = "vulkan",
    any(target_os = "linux", target_os = "windows", target_os = "android")
))]
unsafe extern "C" fn terminate_gpu_process() -> ! {
    std::process::abort()
}

#[no_mangle]
pub extern "C" fn wr_unified_owner_new(
    is_gpu_process: bool,
    vulkan_selected: bool,
) -> *mut WrUnifiedOwner {
    if !is_gpu_process || !vulkan_selected {
        return std::ptr::null_mut();
    }
    #[cfg(all(
        feature = "vulkan",
        any(target_os = "linux", target_os = "windows", target_os = "android")
    ))]
    {
        let factory = std::sync::Arc::new(webrender::vulkan::GraphicsRecorderFactory);
        return match unsafe { wgpu_bindings::unified::bootstrap(factory, terminate_gpu_process) } {
            Ok(owner) => Box::into_raw(Box::new(WrUnifiedOwner { owner })),
            Err(error) => {
                warn!("Unified native owner unavailable: {}", error);
                std::ptr::null_mut()
            }
        };
    }
    #[cfg(not(all(
        feature = "vulkan",
        any(target_os = "linux", target_os = "windows", target_os = "android")
    )))]
    {
        std::ptr::null_mut()
    }
}

#[no_mangle]
pub extern "C" fn wr_unified_owner_is_ready(owner: Option<&WrUnifiedOwner>) -> bool {
    #[cfg(all(
        feature = "vulkan",
        any(target_os = "linux", target_os = "windows", target_os = "android")
    ))]
    {
        owner.map_or(false, |owner| owner.owner.is_ready())
    }
    #[cfg(not(all(
        feature = "vulkan",
        any(target_os = "linux", target_os = "windows", target_os = "android")
    )))]
    {
        let _ = owner;
        false
    }
}

#[no_mangle]
pub extern "C" fn wr_unified_owner_begin_shutdown(owner: Option<&WrUnifiedOwner>) {
    #[cfg(all(
        feature = "vulkan",
        any(target_os = "linux", target_os = "windows", target_os = "android")
    ))]
    {
        if let Some(owner) = owner {
            owner.owner.begin_shutdown();
        }
    }
    #[cfg(not(all(
        feature = "vulkan",
        any(target_os = "linux", target_os = "windows", target_os = "android")
    )))]
    {
        let _ = owner;
    }
}

#[no_mangle]
pub unsafe extern "C" fn wr_unified_owner_delete(owner: *mut WrUnifiedOwner) {
    if !owner.is_null() {
        unsafe {
            drop(Box::from_raw(owner));
        }
    }
}
