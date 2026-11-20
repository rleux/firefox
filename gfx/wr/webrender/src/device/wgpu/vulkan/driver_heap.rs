/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ash::vk;
use crate::internal_types::FastHashMap;
use std::alloc::{alloc, dealloc, Layout};
use std::ffi::c_void;
use std::ptr;
use std::sync::{LazyLock, Mutex};

static ALLOCATIONS: LazyLock<Mutex<FastHashMap<usize, Layout>>> = LazyLock::new(Mutex::default);

pub(super) const CALLBACKS: vk::AllocationCallbacks<'static> = vk::AllocationCallbacks {
    p_user_data: ptr::null_mut(),
    pfn_allocation: Some(allocate),
    pfn_reallocation: Some(reallocate),
    pfn_free: Some(free),
    pfn_internal_allocation: None,
    pfn_internal_free: None,
    _marker: std::marker::PhantomData,
};

pub(in crate::device::wgpu) fn size_of(
    malloc_size_of: unsafe extern "C" fn(*const c_void) -> usize,
) -> usize {
    ALLOCATIONS
        .lock()
        .unwrap()
        .keys()
        .map(|&ptr| unsafe { malloc_size_of(ptr as *const c_void) })
        .sum()
}

unsafe extern "system" fn allocate(
    _: *mut c_void,
    size: usize,
    alignment: usize,
    _: vk::SystemAllocationScope,
) -> *mut c_void {
    let Ok(layout) = Layout::from_size_align(size, alignment) else {
        return ptr::null_mut();
    };
    if size == 0 {
        return ptr::null_mut();
    }
    let mut allocations = ALLOCATIONS.lock().unwrap();
    if allocations.try_reserve(1).is_err() {
        return ptr::null_mut();
    }
    let ptr = unsafe { alloc(layout) };
    if !ptr.is_null() {
        allocations.insert(ptr as usize, layout);
    }
    ptr.cast()
}

unsafe extern "system" fn reallocate(
    user_data: *mut c_void,
    original: *mut c_void,
    size: usize,
    alignment: usize,
    scope: vk::SystemAllocationScope,
) -> *mut c_void {
    if original.is_null() {
        return unsafe { allocate(user_data, size, alignment, scope) };
    }
    if size == 0 {
        unsafe { free(user_data, original) };
        return ptr::null_mut();
    }
    let Ok(layout) = Layout::from_size_align(size, alignment) else {
        return ptr::null_mut();
    };
    let mut allocations = ALLOCATIONS.lock().unwrap();
    let original_layout = allocations[&(original as usize)];
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(original.cast::<u8>(), ptr, size.min(original_layout.size()));
        dealloc(original.cast(), original_layout);
    }
    allocations.remove(&(original as usize));
    allocations.insert(ptr as usize, layout);
    ptr.cast()
}

unsafe extern "system" fn free(_: *mut c_void, ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let mut allocations = ALLOCATIONS.lock().unwrap();
    let layout = allocations.remove(&(ptr as usize)).unwrap();
    unsafe { dealloc(ptr.cast(), layout) };
}
