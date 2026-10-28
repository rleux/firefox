use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use ash::vk;
use core::{fmt, ptr, sync::atomic::Ordering, time::Duration};

use super::super::{semaphore_list::SemaphoreType, CommandBuffer, Fence, Queue};
use super::{NativeDevice, QueueProgress};
use crate::shared::{
    SharedQueueLane, SharedSubmissionReceipt, SharedSubmitError, SharedSubmitSegment,
};

fn rejected(error: crate::DeviceError) -> SharedSubmitError {
    SharedSubmitError {
        error,
        receipt: None,
    }
}

pub(in crate::vulkan) unsafe fn submit_single(
    queue: &Queue,
    buffers: &[&CommandBuffer],
    surfaces: &[&super::super::SurfaceTexture],
    signal: (&Fence, crate::FenceValue),
    generations: &[Arc<super::super::swapchain::Generation>],
) -> Result<(), crate::DeviceError> {
    let progress = queue.progress.as_ref().unwrap();
    let previous = progress.accepted();
    let next = progress.next()?;
    let wait = if previous == 0 {
        Vec::new()
    } else {
        vec![SemaphoreType::Timeline(progress.semaphore, previous)]
    };
    let signals = [SemaphoreType::Timeline(progress.semaphore, next)];
    for generation in generations {
        generation.submission_pending();
    }
    unsafe { queue.submit_impl(buffers, surfaces, Some(signal), &wait, &[], &signals) }?;
    progress.commit(next);
    for generation in generations {
        generation.submission_accepted(progress, next);
    }
    Ok(())
}

struct Receipt {
    terminal: Arc<QueueProgress>,
    value: u64,
    accepted: bool,
    crossings: Vec<vk::Semaphore>,
    progress_targets: Vec<(Arc<QueueProgress>, u64)>,
    _synchronization: crate::external_sync::SubmissionSync,
}

impl fmt::Debug for Receipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedSubmissionReceipt")
            .field("accepted", &self.accepted)
            .field("value", &self.value)
            .finish()
    }
}

impl SharedSubmissionReceipt for Receipt {
    fn poll(&self) -> Result<bool, crate::DeviceError> {
        self.wait(Some(Duration::ZERO))
    }
    fn wait(&self, timeout: Option<Duration>) -> Result<bool, crate::DeviceError> {
        if !self.accepted {
            return Err(crate::DeviceError::Unexpected);
        }
        let semaphores: Vec<_> = self
            .progress_targets
            .iter()
            .map(|(progress, _)| progress.semaphore)
            .collect();
        let values: Vec<_> = self
            .progress_targets
            .iter()
            .map(|(_, value)| *value)
            .collect();
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&semaphores)
            .values(&values);
        let timeout_ns = timeout
            .unwrap_or(Duration::MAX)
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        let device = &self.terminal.device;
        let result = match device.extension_fns.timeline_semaphore {
            Some(super::super::ExtensionFn::Promoted) => unsafe {
                device.raw.wait_semaphores(&info, timeout_ns)
            },
            Some(super::super::ExtensionFn::Extension(ref extension)) => unsafe {
                extension.wait_semaphores(&info, timeout_ns)
            },
            None => unreachable!(),
        };
        match result {
            Ok(()) => Ok(true),
            Err(vk::Result::TIMEOUT) => Ok(false),
            Err(error) => Err(super::super::map_host_device_oom_and_lost_err(error)),
        }
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        for semaphore in self.crossings.drain(..) {
            unsafe { self.terminal.device.raw.destroy_semaphore(semaphore, None) };
        }
    }
}

struct Segment<'a> {
    queue: &'a Queue,
    buffers: Vec<&'a CommandBuffer>,
    incoming: Option<vk::Semaphore>,
    outgoing: Option<vk::Semaphore>,
    synchronization: super::super::external_sync::Prepared,
}

fn same_context(context: &NativeDevice, queue: &Queue) -> bool {
    queue.device.raw.handle() == context.raw.handle()
        && queue
            .device
            .shared_queue_group
            .as_ref()
            .is_some_and(|group| Arc::ptr_eq(group, &context.queues))
}

pub(super) unsafe fn submit_ordered(
    context: &NativeDevice,
    logical_queue: &dyn crate::DynQueue,
    segments: &[SharedSubmitSegment<'_>],
    terminal: (&dyn crate::DynFence, crate::FenceValue),
) -> Result<Box<dyn SharedSubmissionReceipt>, SharedSubmitError> {
    let bad = || rejected(crate::DeviceError::Unexpected);
    let logical = logical_queue
        .as_any()
        .downcast_ref::<Queue>()
        .ok_or_else(bad)?;
    if !same_context(context, logical) || logical.device.queue_index != 1 {
        return Err(bad());
    }
    let fence = terminal
        .0
        .as_any()
        .downcast_ref::<Fence>()
        .ok_or_else(bad)?;
    if !matches!(fence, Fence::TimelineSemaphore(_)) {
        return Err(bad());
    }
    let _graphics_host = context.queues.host[0].lock();
    let _compute_host = context.queues.host[1].lock();
    if context.queues.closed.load(Ordering::Acquire) || logical.has_legacy_pending_sync() {
        return Err(bad());
    }

    let empty_sync = crate::external_sync::SubmissionSync::default();
    let mut signal_floor = Vec::new();
    let mut binary_waits = Vec::new();
    let mut prepared = Vec::with_capacity(segments.len().max(1));
    let mut graphics = None;
    for segment in segments {
        let queue = segment
            .queue
            .as_any()
            .downcast_ref::<Queue>()
            .ok_or_else(bad)?;
        if !same_context(context, queue) || queue.has_legacy_pending_sync() {
            return Err(bad());
        }
        match segment.lane {
            SharedQueueLane::Compute if !ptr::eq(queue, logical) => return Err(bad()),
            SharedQueueLane::Graphics => {
                if queue.device.queue_index != 0 {
                    return Err(bad());
                }
                if let Some(previous) = graphics {
                    if !ptr::eq(previous, queue) {
                        return Err(bad());
                    }
                }
                graphics = Some(queue);
            }
            _ => {}
        }
        let buffers = segment
            .command_buffers
            .iter()
            .map(|buffer| {
                let buffer = buffer
                    .as_any()
                    .downcast_ref::<CommandBuffer>()
                    .ok_or_else(bad)?;
                if buffer.device != context.raw.handle()
                    || buffer.family_index != queue.family_index
                {
                    return Err(bad());
                }
                Ok(buffer)
            })
            .collect::<Result<Vec<_>, SharedSubmitError>>()?;
        prepared.push(Segment {
            queue,
            buffers,
            incoming: None,
            outgoing: None,
            synchronization: super::super::external_sync::prepare(
                queue,
                segment.synchronization.unwrap_or(&empty_sync),
                &mut signal_floor,
                &mut binary_waits,
            )
            .map_err(|_| bad())?,
        });
    }
    if prepared.is_empty() {
        prepared.push(Segment {
            queue: logical,
            buffers: Vec::new(),
            incoming: None,
            outgoing: None,
            synchronization: super::super::external_sync::prepare(
                logical,
                &empty_sync,
                &mut signal_floor,
                &mut binary_waits,
            )
            .map_err(|_| bad())?,
        });
    }

    let logical_progress = logical.progress.as_ref().unwrap();
    let mut reservations: Vec<(Arc<QueueProgress>, u64)> = Vec::new();
    let mut reserve = |progress: &Arc<QueueProgress>| -> Result<(), SharedSubmitError> {
        if let Some((_, count)) = reservations
            .iter_mut()
            .find(|(saved, _)| Arc::ptr_eq(saved, progress))
        {
            *count = count.checked_add(1).ok_or_else(bad)?;
            progress.accepted().checked_add(*count).ok_or_else(bad)?;
        } else {
            progress.next().map_err(rejected)?;
            reservations.push((progress.clone(), 1));
        }
        Ok(())
    };
    for segment in &prepared {
        reserve(segment.queue.progress.as_ref().unwrap())?;
    }
    if !ptr::eq(prepared.last().unwrap().queue, logical) {
        reserve(logical_progress)?;
    }
    let terminal_count = reservations
        .iter()
        .find(|(progress, _)| Arc::ptr_eq(progress, logical_progress))
        .unwrap()
        .1;
    let mut retained_sync = crate::external_sync::SubmissionSync::default();
    for segment in &prepared {
        retained_sync
            .waits
            .extend(segment.synchronization.leases.waits.iter().cloned());
        retained_sync
            .signals
            .extend(segment.synchronization.leases.signals.iter().cloned());
    }
    let mut receipt = Receipt {
        terminal: logical_progress.clone(),
        value: logical_progress.accepted() + terminal_count,
        accepted: false,
        crossings: Vec::new(),
        _synchronization: retained_sync,
        progress_targets: reservations
            .into_iter()
            .map(|(progress, count)| {
                let target = progress.accepted() + count;
                (progress, target)
            })
            .collect(),
    };
    for index in 1..prepared.len() {
        if prepared[index - 1].queue.device.queue_index != prepared[index].queue.device.queue_index
        {
            let semaphore = logical
                .device
                .new_binary_semaphore("shared ordered crossing")
                .map_err(rejected)?;
            receipt.crossings.push(semaphore);
            prepared[index - 1].outgoing = Some(semaphore);
            prepared[index].incoming = Some(semaphore);
        }
    }

    for segment in &mut prepared {
        segment.synchronization.reserve().map_err(|_| bad())?;
    }
    let segment_count = prepared.len();
    let mut failure = None;
    for (index, segment) in prepared.iter_mut().enumerate() {
        #[cfg(test)]
        if let Err(error) = tests::before_segment(index) {
            failure = Some(error);
            break;
        }
        let last = index + 1 == segment_count;
        let progress = segment.queue.progress.as_ref().unwrap();
        let previous = progress.accepted();
        let next = progress.next().unwrap();
        let mut waits = segment.synchronization.waits.clone();
        if previous != 0 {
            waits.push(SemaphoreType::Timeline(progress.semaphore, previous));
        }
        if (index == 0 || last) && !ptr::eq(segment.queue, logical) {
            // Crossing signals precede the producer's completion marker.
            let previous = logical_progress.accepted();
            if previous != 0 {
                waits.push(SemaphoreType::Timeline(
                    logical_progress.semaphore,
                    previous,
                ));
            }
        }
        if let Some(semaphore) = segment.incoming {
            waits.push(SemaphoreType::Binary(semaphore));
        }
        let mut signals = segment.synchronization.signals.clone();
        let mut completion = vec![SemaphoreType::Timeline(progress.semaphore, next)];
        if let Some(semaphore) = segment.outgoing {
            signals.push(SemaphoreType::Binary(semaphore));
        }
        let logical_terminal =
            (last && !ptr::eq(segment.queue, logical)).then(|| logical_progress.next().unwrap());
        if let Some(value) = logical_terminal {
            completion.push(SemaphoreType::Timeline(logical_progress.semaphore, value));
        }
        let signal = last.then_some((fence, terminal.1));
        segment.synchronization.attempted();
        match unsafe {
            segment
                .queue
                .submit_impl(&segment.buffers, &[], signal, &waits, &signals, &completion)
        } {
            Ok(()) => {
                #[cfg(test)]
                if let Err(error) = tests::after_native_segment(index) {
                    failure = Some(error);
                    break;
                }
                segment.synchronization.accepted();
                progress.commit(next);
                log::info!(
                    "shared-segment-accepted pid={} device={} lane={} queue_index={} command_buffers={}",
                    std::process::id(), context.queues.log_device_id,
                    if segment.queue.device.queue_index == 0 { "graphics" } else { "compute" },
                    segment.queue.device.queue_index, segment.buffers.len()
                );
                if let Some(value) = logical_terminal {
                    logical_progress.commit(value);
                }
            }
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    if let Some(error) = failure {
        context.queues.close_uncertain();
        return Err(SharedSubmitError {
            error,
            receipt: Some(Box::new(receipt)),
        });
    }
    receipt.accepted = true;
    Ok(Box::new(receipt))
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;
    std::thread_local! {
        static FAIL_BEFORE_SEGMENT: Cell<Option<usize>> = const { Cell::new(None) };
        static FAIL_AFTER_NATIVE: Cell<Option<usize>> = const { Cell::new(None) };
    }
    pub(in crate::vulkan::shared) fn inject_failure(index: usize) {
        FAIL_BEFORE_SEGMENT.set(Some(index));
    }
    pub(in crate::vulkan::shared) fn inject_after_native(index: usize) {
        FAIL_AFTER_NATIVE.set(Some(index));
    }
    pub(super) fn after_native_segment(index: usize) -> Result<(), crate::DeviceError> {
        if FAIL_AFTER_NATIVE.get() == Some(index) {
            FAIL_AFTER_NATIVE.set(None);
            Err(crate::DeviceError::OutOfMemory)
        } else {
            Ok(())
        }
    }
    pub(super) fn before_segment(index: usize) -> Result<(), crate::DeviceError> {
        if FAIL_BEFORE_SEGMENT.get() == Some(index) {
            FAIL_BEFORE_SEGMENT.set(None);
            Err(crate::DeviceError::OutOfMemory)
        } else {
            Ok(())
        }
    }
}
#[cfg(test)]
pub(super) use tests::{inject_after_native, inject_failure};

pub(super) unsafe fn submit_graphics(
    context: &NativeDevice,
    queue: &dyn crate::DynQueue,
    buffers: &[&dyn crate::DynCommandBuffer],
    surfaces: &[&dyn crate::DynSurfaceTexture],
    terminal: (&dyn crate::DynFence, crate::FenceValue),
    synchronization: &crate::external_sync::SubmissionSync,
) -> Result<Box<dyn SharedSubmissionReceipt>, SharedSubmitError> {
    let bad = || rejected(crate::DeviceError::Unexpected);
    let queue = queue.as_any().downcast_ref::<Queue>().ok_or_else(bad)?;
    if !same_context(context, queue)
        || queue.device.queue_index != 0
        || (!surfaces.is_empty() && !context.queues.presentation_fences_enabled)
    {
        return Err(bad());
    }
    let fence = terminal
        .0
        .as_any()
        .downcast_ref::<Fence>()
        .ok_or_else(bad)?;
    if !matches!(fence, Fence::TimelineSemaphore(_)) {
        return Err(bad());
    }
    #[cfg(test)]
    if !surfaces.is_empty() {
        context
            .queues
            .surface_submit_waiters
            .fetch_add(1, Ordering::SeqCst);
    }
    let _host = context.queues.host[0].lock();
    #[cfg(test)]
    if !surfaces.is_empty() {
        context
            .queues
            .surface_submit_waiters
            .fetch_sub(1, Ordering::SeqCst);
    }
    if context.queues.closed.load(Ordering::Acquire) || queue.has_legacy_pending_sync() {
        return Err(bad());
    }
    let buffers = buffers
        .iter()
        .map(|buffer| {
            let buffer = buffer
                .as_any()
                .downcast_ref::<CommandBuffer>()
                .ok_or_else(bad)?;
            if buffer.device != context.raw.handle() || buffer.family_index != queue.family_index {
                return Err(bad());
            }
            Ok(buffer)
        })
        .collect::<Result<Vec<_>, SharedSubmitError>>()?;
    let surfaces = surfaces
        .iter()
        .map(|surface| {
            surface
                .as_any()
                .downcast_ref::<super::super::SurfaceTexture>()
                .ok_or_else(bad)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let generations = super::super::swapchain::validate_shared_submission(queue, &surfaces, fence)
        .map_err(rejected)?;
    let mut sync = super::super::external_sync::prepare(
        queue,
        synchronization,
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .map_err(|_| bad())?;
    let progress = queue.progress.as_ref().unwrap();
    let previous = progress.accepted();
    let next = progress.next().map_err(rejected)?;
    let mut waits = sync.waits.clone();
    if previous != 0 {
        waits.push(SemaphoreType::Timeline(progress.semaphore, previous));
    }
    let completion = [SemaphoreType::Timeline(progress.semaphore, next)];
    let mut receipt = Receipt {
        terminal: progress.clone(),
        value: next,
        accepted: false,
        crossings: Vec::new(),
        progress_targets: vec![(progress.clone(), next)],
        _synchronization: synchronization.clone(),
    };
    sync.reserve().map_err(|_| bad())?;
    sync.attempted();
    for generation in &generations {
        generation.submission_pending();
    }
    if let Err(error) = unsafe {
        queue.submit_impl(
            &buffers,
            &surfaces,
            Some((fence, terminal.1)),
            &waits,
            &sync.signals,
            &completion,
        )
    } {
        context.queues.close_uncertain();
        return Err(SharedSubmitError {
            error,
            receipt: Some(Box::new(receipt)),
        });
    }
    sync.accepted();
    progress.commit(next);
    for generation in generations {
        generation.submission_accepted(progress, next);
    }
    receipt.accepted = true;
    Ok(Box::new(receipt))
}
