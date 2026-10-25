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
    signal: (&Fence, crate::FenceValue),
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
    unsafe { queue.submit_impl(buffers, &[], Some(signal), &wait, &[], &signals) }?;
    progress.commit(next);
    Ok(())
}

struct Receipt {
    terminal: Arc<QueueProgress>,
    value: u64,
    accepted: bool,
    crossings: Vec<vk::Semaphore>,
    progress_targets: Vec<(Arc<QueueProgress>, u64)>,
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
    if context.queues.closed.load(Ordering::Acquire) {
        return Err(bad());
    }

    let mut prepared = Vec::with_capacity(segments.len().max(1));
    let mut graphics = None;
    for segment in segments {
        let queue = segment
            .queue
            .as_any()
            .downcast_ref::<Queue>()
            .ok_or_else(bad)?;
        if !same_context(context, queue) {
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
        });
    }
    if prepared.is_empty() {
        prepared.push(Segment {
            queue: logical,
            buffers: Vec::new(),
            incoming: None,
            outgoing: None,
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
    let mut receipt = Receipt {
        terminal: logical_progress.clone(),
        value: logical_progress.accepted() + terminal_count,
        accepted: false,
        crossings: Vec::new(),
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

    let mut failure = None;
    for (index, segment) in prepared.iter().enumerate() {
        #[cfg(test)]
        if let Err(error) = tests::before_segment(index) {
            failure = Some(error);
            break;
        }
        let last = index + 1 == prepared.len();
        let progress = segment.queue.progress.as_ref().unwrap();
        let previous = progress.accepted();
        let next = progress.next().unwrap();
        let mut waits = Vec::new();
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
        let mut signals = Vec::new();
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
                progress.commit(next);
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
