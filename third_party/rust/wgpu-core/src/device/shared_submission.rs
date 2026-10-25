use super::*;
use crate::graphics::{GraphicsSubmissionProvider, GraphicsSubmitPart, NativeRetirement};

fn seal_graphics(
    executions: &mut [EncoderInFlight],
    provider: &dyn GraphicsSubmissionProvider,
) -> Result<(), hal::DeviceError> {
    for execution in executions {
        provider.seal_encoder(execution.inner.raw.as_mut())?;
    }
    Ok(())
}

fn submit_parts<'a>(
    executions: &'a [EncoderInFlight],
    queue: &'a dyn hal::DynQueue,
    provider: &dyn GraphicsSubmissionProvider,
) -> Result<Vec<GraphicsSubmitPart<'a>>, hal::DeviceError> {
    let mut parts = Vec::new();
    for execution in executions {
        parts.extend(provider.submission_parts(
            execution.inner.raw.as_ref(),
            &execution.inner.list,
            queue,
        )?);
    }
    Ok(parts)
}

impl Queue {
    pub(super) fn submit_shared_pending<'a>(
        &self,
        pending_writes: MutexGuard<'_, PendingWrites>,
        command_index_guard: RwLockWriteGuard<'a, CommandIndices>,
        snatch_guard: SnatchGuard<'a>,
        mut executions: Vec<EncoderInFlight>,
        surface_textures: FastHashMap<*const Texture, Arc<Texture>>,
        submit_index: SubmissionIndex,
    ) -> Result<SubmissionResult<'a>, DeviceError> {
        let provider = self.device.graphics_submission.as_ref().unwrap().clone();
        let queue_lease = self
            .device
            .get_queue()
            .filter(|queue| core::ptr::eq(Arc::as_ptr(queue), self));
        let prepared = if queue_lease.is_none() || !surface_textures.is_empty() {
            Err(hal::DeviceError::Unexpected)
        } else {
            seal_graphics(&mut executions, provider.as_ref())
        };
        let result = match prepared
            .and_then(|()| submit_parts(&executions, self.raw(), provider.as_ref()))
        {
            Ok(parts) => {
                let buffers: Vec<_> = parts.iter().map(|part| [part.buffer]).collect();
                let segments: Vec<_> = parts
                    .iter()
                    .zip(&buffers)
                    .map(|(part, buffers)| hal::shared::SharedSubmitSegment {
                        lane: part.lane,
                        queue: part.queue,
                        command_buffers: buffers,
                    })
                    .collect();
                unsafe {
                    provider.submit(
                        self.raw(),
                        &segments,
                        (self.device.fence.as_ref(), submit_index),
                    )
                }
            }
            Err(error) => Err(hal::shared::SharedSubmitError {
                error,
                receipt: None,
            }),
        };
        match result {
            Ok(receipt) => {
                let receipt: Arc<dyn hal::shared::SharedSubmissionReceipt> = Arc::from(receipt);
                drop(pending_writes);
                self.device
                    .last_successful_submission_index
                    .fetch_max(submit_index, Ordering::SeqCst);
                self.lock_life().track_shared_submission(
                    submit_index,
                    executions,
                    Some(receipt),
                    false,
                );
                drop(command_index_guard);
                Ok(SubmissionResult { snatch_guard })
            }
            Err(error) => {
                let had_receipt = error.receipt.is_some();
                let receipt = error
                    .receipt
                    .map(Arc::<dyn hal::shared::SharedSubmissionReceipt>::from);
                let retirement = if had_receipt {
                    match provider.drain() {
                        Ok(()) | Err(hal::DeviceError::Lost) => NativeRetirement::Confirmed,
                        Err(_) => NativeRetirement::Unconfirmed,
                    }
                } else {
                    NativeRetirement::Confirmed
                };
                self.device.valid.store(false, Ordering::Release);
                drop(pending_writes);
                if retirement == NativeRetirement::Unconfirmed {
                    self.lock_life().track_shared_submission(
                        submit_index,
                        executions,
                        receipt,
                        true,
                    );
                    // Keep Device's weak Queue link usable for subsequent explicit resource destruction.
                    core::mem::forget(queue_lease.as_ref().unwrap().clone());
                } else {
                    drop((executions, receipt));
                }
                drop(command_index_guard);
                drop(snatch_guard);
                let mapped = self.device.handle_hal_error(error.error);
                if had_receipt {
                    provider.request_recovery(retirement);
                }
                Err(mapped)
            }
        }
    }
}

impl Queue {
    fn receipt_snapshots(
        &self,
        through: SubmissionIndex,
    ) -> Vec<(
        SubmissionIndex,
        Arc<dyn hal::shared::SharedSubmissionReceipt>,
    )> {
        let _indices = self.device.command_indices.read();
        let snapshots = self.lock_life().receipt_snapshots(through);
        snapshots
    }

    fn update_receipts(
        &self,
        through: SubmissionIndex,
        wait: bool,
        timeout: Option<core::time::Duration>,
        start: std::time::Instant,
    ) -> Result<bool, hal::DeviceError> {
        for (index, receipt) in self.receipt_snapshots(through) {
            let ready = if wait {
                receipt.wait(timeout.map(|duration| duration.saturating_sub(start.elapsed())))?
            } else {
                receipt.poll()?
            };
            if !ready {
                return Ok(false);
            }
            self.lock_life().mark_receipt_ready(index);
        }
        Ok(true)
    }

    fn append_maintained(
        &self,
        through: SubmissionIndex,
        snatch: &SnatchGuard,
        closures: &mut crate::device::UserClosures,
    ) -> bool {
        let (submissions, mappings, blas, empty) = self.maintain(through, snatch);
        closures.submissions.extend(submissions);
        closures.mappings.extend(mappings);
        closures.blas_compact_ready.extend(blas);
        empty
    }

    fn shared_poll_failure(
        self: &Arc<Self>,
        error: hal::DeviceError,
        snatch: SnatchGuard,
        mut closures: crate::device::UserClosures,
    ) -> (
        crate::device::UserClosures,
        Result<wgt::PollStatus, WaitIdleError>,
    ) {
        let indices = self.device.command_indices.write();
        if self.lock_life().has_unconfirmed() {
            drop(indices);
            drop(snatch);
            return (closures, Err(DeviceError::Lost.into()));
        }
        let provider = self.device.graphics_submission.as_ref().unwrap().clone();
        self.device.valid.store(false, Ordering::Release);
        let retirement = match provider.drain() {
            Ok(()) | Err(hal::DeviceError::Lost) => NativeRetirement::Confirmed,
            Err(_) => NativeRetirement::Unconfirmed,
        };
        let last = self
            .device
            .last_successful_submission_index
            .load(Ordering::Acquire);
        if retirement == NativeRetirement::Confirmed {
            self.lock_life().confirm_drained();
            self.append_maintained(u64::MAX, &snatch, &mut closures);
        } else {
            self.append_maintained(last, &snatch, &mut closures);
            self.lock_life().quarantine_active();
            core::mem::forget(self.clone());
        }
        drop(indices);
        drop(snatch);
        let mapped = self.device.handle_hal_error(error);
        provider.request_recovery(retirement);
        (closures, Err(mapped.into()))
    }

    pub(crate) fn maintain_shared(
        self: Arc<Self>,
        poll_type: wgt::PollType<SubmissionIndex>,
        snatch: SnatchGuard,
        mut closures: crate::device::UserClosures,
    ) -> (
        crate::device::UserClosures,
        Result<wgt::PollStatus, WaitIdleError>,
    ) {
        let start = std::time::Instant::now();
        let (target, timeout) = {
            let indices = self.device.command_indices.read();
            let last = self
                .device
                .last_successful_submission_index
                .load(Ordering::Acquire);
            match poll_type {
                wgt::PollType::Poll => (None, None),
                wgt::PollType::Wait {
                    submission_index,
                    timeout,
                } => {
                    let target = submission_index.unwrap_or(last);
                    if target > last {
                        drop(indices);
                        drop(snatch);
                        return (
                            closures,
                            Err(WaitIdleError::WrongSubmissionIndex(target, last)),
                        );
                    }
                    (Some(target), timeout)
                }
            }
        };
        if let Some(target) = target {
            let waited = unsafe {
                self.device.raw().wait(
                    self.device.fence.as_ref(),
                    target,
                    timeout.map(|duration| duration.saturating_sub(start.elapsed())),
                )
            };
            match waited {
                Err(error) => return self.shared_poll_failure(error, snatch, closures),
                Ok(true) => {
                    if let Err(error) = self.update_receipts(target, true, timeout, start) {
                        return self.shared_poll_failure(error, snatch, closures);
                    }
                }
                Ok(false) => {}
            }
        }
        let done = match unsafe {
            self.device
                .raw()
                .get_fence_value(self.device.fence.as_ref())
        } {
            Ok(done) => done,
            Err(error) => return self.shared_poll_failure(error, snatch, closures),
        };
        if let Err(error) = self.update_receipts(done, false, None, start) {
            return self.shared_poll_failure(error, snatch, closures);
        }
        let valid = {
            let _indices = self.device.command_indices.read();
            self.device.is_valid()
        };
        let empty = self.append_maintained(done, &snatch, &mut closures);
        let target_ready = target.is_none_or(|target| self.lock_life().submission_complete(target));
        drop(snatch);
        if !valid && empty {
            if let Some(closure) = self.device.device_lost_closure.lock().take() {
                closures
                    .device_lost_invocations
                    .push(crate::device::DeviceLostInvocation {
                        closure,
                        reason: wgt::DeviceLostReason::Destroyed,
                        message: String::new(),
                    });
            }
            self.device.release_gpu_resources();
        }
        let result = Self::shared_poll_status(done, target, empty, target_ready);
        (closures, result)
    }

    pub(crate) fn shared_poll_status(
        done: SubmissionIndex,
        target: Option<SubmissionIndex>,
        empty: bool,
        target_retired: bool,
    ) -> Result<wgt::PollStatus, WaitIdleError> {
        match target {
            Some(target) if done < target || !target_retired => Err(WaitIdleError::Timeout),
            _ if empty => Ok(wgt::PollStatus::QueueEmpty),
            Some(_) => Ok(wgt::PollStatus::WaitSucceeded),
            None => Ok(wgt::PollStatus::Poll),
        }
    }

    pub(super) fn drop_shared(&mut self) {
        let provider = self.device.graphics_submission.as_ref().unwrap().clone();
        let fatal = self.device.graphics_termination.unwrap();
        let snatch = self.device.snatchable_lock.read();
        let last = self
            .device
            .last_successful_submission_index
            .load(Ordering::Acquire);
        let waited = unsafe { self.raw.wait_for_idle() }.and_then(|()| {
            if self.lock_life().has_unconfirmed() {
                return Err(hal::DeviceError::Unexpected);
            }
            if self.update_receipts(last, true, None, std::time::Instant::now())? {
                Ok(())
            } else {
                Err(hal::DeviceError::Unexpected)
            }
        });
        let failed = waited.err();
        let through = if failed.is_some() {
            self.device.valid.store(false, Ordering::Release);
            match provider.drain() {
                Ok(()) | Err(hal::DeviceError::Lost) => {
                    self.lock_life().confirm_drained();
                    u64::MAX
                }
                Err(_) => unsafe { fatal() },
            }
        } else {
            last
        };
        let mut closures = crate::device::UserClosures::default();
        let empty = self.append_maintained(through, &snatch, &mut closures);
        drop(snatch);
        if let Some(error) = failed {
            self.device.handle_hal_error(error);
            provider.request_recovery(NativeRetirement::Confirmed);
        }
        assert!(empty);
        closures.fire();
    }
}
