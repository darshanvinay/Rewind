//! Bounded orchestration; persistence remains in snapshot.
use super::*;
use crate::snapshot::AutomaticCapture;
use std::{
    thread::{self, JoinHandle},
    time::Duration,
};

struct CaptureJob {
    generation: u64,
    reason: &'static str,
    dirty: crate::shadow::DirtyPlan,
}
struct Completion {
    job: CaptureJob,
    result: Result<AutomaticCapture>,
    elapsed: Duration,
    started: Instant,
    shadow: serde_json::Value,
}
struct CaptureWorker {
    jobs: Option<SyncSender<CaptureJob>>,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}
impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn json_line(output: &mut impl Write, value: serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *output, &value)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}
fn retryable(error: &Error) -> bool {
    matches!(error, Error::Unstable(_))
        || matches!(error, Error::SourceIo(e) if matches!(e.kind(),
        std::io::ErrorKind::NotADirectory | std::io::ErrorKind::NotFound | std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied))
}
impl Monitor {
    /// Foreground automatic history. The event-only `run` API remains available.
    pub fn run_automatic(self, output: &mut impl Write) -> Result<()> {
        self.run_automatic_with_reconciliation(output, Duration::from_secs(60))
    }

    /// Periodic authoritative scans also detect changes without usable events.
    /// The interval is a scheduling target, not a maximum staleness guarantee.
    pub fn run_automatic_with_reconciliation(
        self,
        output: &mut impl Write,
        interval: Duration,
    ) -> Result<()> {
        if !(Duration::from_secs(1)..=Duration::from_secs(86400)).contains(&interval) {
            return Err(Error::Invalid(
                "reconciliation interval must be 1..86400 seconds".into(),
            ));
        }
        let (jobs_tx, jobs_rx) = mpsc::sync_channel::<CaptureJob>(1);
        let (result_tx, results) = mpsc::sync_channel::<Completion>(1);
        let store_path = self.source.store_root();
        let source_path = self.source.root.clone();
        let identity = self.source.root_identity();
        let stop = self.stop.stopped.clone();
        let capture_stop = stop.clone();
        let wake = self.stop.sender.clone();
        #[cfg(test)]
        let mut test_hook = tests::WORKER_HOOK.with(|h| h.borrow_mut().take());
        let capture = thread::spawn(move || {
            let mut cache = crate::shadow::MerkleCache::default();
            while let Ok(job) = jobs_rx.recv() {
                #[cfg(test)]
                if let Some(hook) = test_hook.take() {
                    hook();
                }
                let start = Instant::now();
                let candidate_start = Instant::now();
                let candidate = cache.candidate(&job.dirty, &capture_stop);
                let candidate_ms = candidate_start.elapsed().as_secs_f64() * 1000.0;
                let validation_start = Instant::now();
                let result = ChunkStore::open(&store_path).and_then(|store| {
                    crate::snapshot::scan::automatic_seeded(
                        &store,
                        &source_path,
                        &capture_stop,
                        Some(identity),
                        Some(cache.limits()),
                    )
                });
                let validation_ms = validation_start.elapsed().as_secs_f64() * 1000.0;
                let mut shadow = match candidate {
                    Ok(crate::shadow::ShadowResult::Candidate(candidate)) => {
                        serde_json::json!({"kind":"shadow","status":"candidate","root_hash":candidate.root_hash,"stats":candidate.stats})
                    }
                    Ok(crate::shadow::ShadowResult::NeedsReconciliation { reason }) => {
                        serde_json::json!({"kind":"shadow","status":"requires_reconciliation","reason":reason})
                    }
                    Err(error) => {
                        serde_json::json!({"kind":"shadow","status":"candidate_error","message":error.to_string()})
                    }
                };
                shadow["candidate_ms"] = candidate_ms.into();
                shadow["full_validation_ms"] = validation_ms.into();
                shadow["publisher"] = "milestone_c".into();
                let result = result.map(|(result, seed)| {
                    let record = match &result {
                        AutomaticCapture::Created(r) | AutomaticCapture::Unchanged(r) => r,
                    };
                    if let Some(candidate) = shadow.get("root_hash").and_then(|v| v.as_str()) {
                        let matches = candidate == record.root_hash();
                        shadow["matches_authoritative"] = matches.into();
                        if !matches {
                            shadow["status"] = "mismatch_discarded".into();
                        }
                    }
                    // Only C's validated seed is installed, never the shadow candidate.
                    cache.install(seed);
                    result
                });
                if result.is_err() {
                    cache.invalidate(crate::shadow::Invalidation::AttemptFailed);
                }
                shadow["cache_entries"] = cache.entry_count().into();
                shadow["cache_charged_bytes"] = cache.charged_bytes().into();
                if result_tx
                    .send(Completion {
                        job,
                        result,
                        elapsed: start.elapsed(),
                        started: start,
                        shadow,
                    })
                    .is_err()
                {
                    break;
                }
                let _ = wake.try_send(Message::Wake); // Full inbox already wakes the controller.
            }
        });
        let worker = CaptureWorker {
            jobs: Some(jobs_tx),
            thread: Some(capture),
            stop,
        };
        json_line(
            output,
            serde_json::json!({"status":"watching","mode":"automatic","root":self.source.root,
            "capture_mode":"strict","reconcile_interval_secs":interval.as_secs(),"backend":format!("{:?}",RecommendedWatcher::kind()),"debounce_ms":self.config.debounce_ms(),"max_delay_ms":self.config.max_delay_ms()}),
        )?;
        let mut debounce = Debouncer::new(self.config);
        let mut pending_plan = crate::shadow::DirtyPlan::full(crate::shadow::Invalidation::Cold);
        let mut next_full = Instant::now() + interval;
        let mut generation = 0_u64;
        let mut active = false;
        let mut ready = true; // Always establish an authoritative startup baseline.
        let mut reason = "baseline";
        let mut failures = 0_u32;
        let mut deferred_cycles = 0_u32;
        let mut retry_at: Option<Instant> = None;
        loop {
            let now = Instant::now();
            if self.overflow.swap(false, Ordering::AcqRel) {
                generation = generation
                    .checked_add(1)
                    .ok_or_else(|| Error::Invalid("event generation exhausted".into()))?;
                pending_plan =
                    crate::shadow::DirtyPlan::full(crate::shadow::Invalidation::EventLoss);
                debounce.reconcile("queue_overflow_or_oversized_event", now);
            }
            if let Ok(done) = results.try_recv() {
                active = false;
                json_line(output, done.shadow)?;
                match done.result {
                    Ok(outcome) => {
                        next_full = done.started + interval;
                        failures = 0;
                        deferred_cycles = 0;
                        retry_at = None;
                        let (kind, record) = match outcome {
                            AutomaticCapture::Created(record) => ("snapshot_created", record),
                            AutomaticCapture::Unchanged(record) => ("skipped_identical", record),
                        };
                        json_line(
                            output,
                            serde_json::json!({"kind":kind,"snapshot_id":record.id(),"root_hash":record.root_hash(),"reason":done.job.reason,
                            "elapsed_ms":done.elapsed.as_secs_f64()*1000.0,"capture_mode":"strict","validation_scope":"full_source","record_source_validation":record.source_validation(),"consistency":"two_pass_validated_observations"}),
                        )?;
                        if generation == done.job.generation
                            && debounce.deadline().is_none()
                            && !ready
                            && !self.overflow.load(Ordering::Acquire)
                        {
                            debounce.acknowledge_reconciliation();
                            json_line(
                                output,
                                serde_json::json!({"kind":"reconciliation","status":"complete","generation":generation}),
                            )?;
                        } else {
                            // A successful capture does not acknowledge newer input.
                            ready = true;
                            reason = "reconciliation";
                        }
                    }
                    Err(Error::Cancelled) => {
                        ready = true;
                        json_line(
                            output,
                            serde_json::json!({"kind":"cancelled","reason":"shutdown"}),
                        )?;
                    }
                    Err(error) if retryable(&error) => {
                        failures += 1;
                        let delay = if failures < 3 {
                            Duration::from_millis(if failures == 1 { 100 } else { 300 })
                        } else {
                            failures = 0;
                            deferred_cycles = (deferred_cycles + 1).min(6);
                            Duration::from_secs((1_u64 << (deferred_cycles - 1)).min(30))
                        };
                        json_line(
                            output,
                            serde_json::json!({"kind":"error","fatal":false,"code":"source_unstable_or_transient","message":error.to_string(),
                            "attempt":if failures==0 {3} else {failures},"deferred":failures==0,"retry_in_ms":delay.as_millis()}),
                        )?;
                        pending_plan = crate::shadow::DirtyPlan::full(
                            crate::shadow::Invalidation::AttemptFailed,
                        );
                        debounce.reconcile("source_retry", now);
                        ready = true;
                        reason = "retry";
                        retry_at = Some(now + delay);
                    }
                    Err(error) => {
                        json_line(
                            output,
                            serde_json::json!({"kind":"error","fatal":true,"code":"integrity_storage_or_source_policy","message":error.to_string()}),
                        )?;
                        return Err(error);
                    }
                }
            }
            if self.stop.stopped.load(Ordering::Relaxed) {
                if active {
                    // Await a safe cancellation/commit boundary, without scheduling work.
                    match self.receiver.recv_timeout(Duration::from_millis(25)) {
                        Ok(_) | Err(RecvTimeoutError::Timeout) => (),
                        Err(_) => break,
                    }
                    continue;
                }
                emit(&mut debounce, output)?;
                json_line(
                    output,
                    serde_json::json!({"status":"stopped","pending_reconciliation":ready || debounce.deadline().is_some()}),
                )?;
                return Ok(());
            }
            if debounce.deadline().is_some_and(|d| now >= d)
                && let Some(batch) = debounce.take()
            {
                pending_plan.merge(batch.dirty_plan());
                reason = if batch.reconciliation_required {
                    "reconciliation"
                } else {
                    "changes"
                };
                json_line(output, serde_json::to_value(batch)?)?;
                ready = true;
            }
            if !active && !ready && now >= next_full {
                pending_plan =
                    crate::shadow::DirtyPlan::full(crate::shadow::Invalidation::Explicit);
                ready = true;
                reason = "periodic";
            }
            if ready && !active && retry_at.is_none_or(|deadline| now >= deadline) {
                if let Err(error) = self.source.check_roots() {
                    json_line(
                        output,
                        serde_json::json!({"kind":"error","fatal":true,"code":"root_or_store_replaced","message":error.to_string()}),
                    )?;
                    return Err(error);
                }
                json_line(
                    output,
                    serde_json::json!({"kind":"reconciliation","status":"started","reason":reason,"generation":generation}),
                )?;
                worker
                    .jobs
                    .as_ref()
                    .unwrap()
                    .send(CaptureJob {
                        generation,
                        reason,
                        dirty: std::mem::take(&mut pending_plan),
                    })
                    .map_err(|_| Error::Invalid("capture worker disconnected".into()))?;
                active = true;
                ready = false;
                retry_at = None;
            }
            let deadline = debounce
                .deadline()
                .into_iter()
                .chain(if !active { retry_at } else { None })
                .chain(if !active && !ready {
                    Some(next_full)
                } else {
                    None
                })
                .min();
            let message = match deadline {
                Some(deadline) => match self
                    .receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(_) => return Err(Error::Invalid("watch inbox disconnected".into())),
                },
                None => Some(
                    self.receiver
                        .recv()
                        .map_err(|_| Error::Invalid("watch inbox disconnected".into()))?,
                ),
            };
            if let Some(Message::Event(event)) = message {
                if let Err(error) = self.source.check_roots() {
                    json_line(
                        output,
                        serde_json::json!({"kind":"error","fatal":true,"code":"root_or_store_replaced","message":error.to_string()}),
                    )?;
                    return Err(error);
                }
                let event = event
                    .map_err(watch_error)
                    .and_then(|e| self.source.normalize(e));
                match event {
                    Ok(Some(event)) => {
                        generation = generation
                            .checked_add(1)
                            .ok_or_else(|| Error::Invalid("event generation exhausted".into()))?;
                        debounce.push(event, Instant::now());
                    }
                    Ok(None) => (),
                    Err(Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::PermissionDenied
                                | std::io::ErrorKind::Interrupted
                                | std::io::ErrorKind::WouldBlock
                        ) =>
                    {
                        generation = generation
                            .checked_add(1)
                            .ok_or_else(|| Error::Invalid("event generation exhausted".into()))?;
                        pending_plan =
                            crate::shadow::DirtyPlan::full(crate::shadow::Invalidation::Ambiguous);
                        debounce.reconcile("source_race", Instant::now());
                        json_line(
                            output,
                            serde_json::json!({"kind":"error","fatal":false,"code":"source_race","message":error.to_string()}),
                        )?;
                    }
                    Err(error) => {
                        json_line(
                            output,
                            serde_json::json!({"kind":"error","fatal":true,"code":"watcher_or_source_policy","message":error.to_string()}),
                        )?;
                        return Err(error);
                    }
                }
            }
        }
        Err(Error::Invalid(
            "watch inbox disconnected during shutdown".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, fs};
    type Hook = Box<dyn FnOnce() + Send>;
    thread_local! { pub(super) static WORKER_HOOK: RefCell<Option<Hook>> = const {RefCell::new(None)}; }
    #[test]
    fn events_and_overflow_during_an_unstable_capture_are_reconciled_after_retry() {
        let temp = tempfile::tempdir().unwrap();
        let store = ChunkStore::init(temp.path().join("store")).unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"before").unwrap();
        let monitor = Monitor::start_with_config(
            &store,
            &source,
            DebounceConfig::from_millis(30, 100).unwrap(),
        )
        .unwrap();
        let stop = monitor.stop_handle();
        let overflow = monitor.overflow.clone();
        let sender = monitor.stop.sender.clone();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        WORKER_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                crate::snapshot::scan::auto_tests::set_probe_hook(Box::new(move |_| {
                    blocked_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                }))
            }))
        });
        let (lines_tx, lines_rx) = mpsc::channel();
        struct Lines {
            bytes: Vec<u8>,
            tx: mpsc::Sender<serde_json::Value>,
        }
        impl Write for Lines {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                for line in std::mem::take(&mut self.bytes)
                    .split(|b| *b == b'\n')
                    .filter(|l| !l.is_empty())
                {
                    self.tx.send(serde_json::from_slice(line).unwrap()).unwrap();
                }
                Ok(())
            }
        }
        // Move the test-only worker setup along with the monitor to its controller.
        let hook = WORKER_HOOK.with(|h| h.borrow_mut().take());
        let controller = thread::spawn(move || {
            WORKER_HOOK.with(|h| *h.borrow_mut() = hook);
            monitor.run_automatic(&mut Lines {
                bytes: vec![],
                tx: lines_tx,
            })
        });
        blocked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        fs::write(source.join("file"), b"during capture").unwrap();
        overflow.store(true, Ordering::Release);
        sender
            .try_send(Message::Event(Ok(
                Event::new(EventKind::Any).set_flag(notify::event::Flag::Rescan)
            )))
            .unwrap();
        release_tx.send(()).unwrap();
        let mut saw_retry = false;
        let mut saw_loss = false;
        let mut snapshot = None;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let value = lines_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if value["kind"] == "error" && value["fatal"] == false {
                saw_retry = true;
            }
            if value["kind"] == "batch" && value["reconciliation_required"] == true {
                saw_loss = true;
            }
            if value["kind"] == "snapshot_created" {
                snapshot = value["snapshot_id"].as_str().map(str::to_owned);
            }
            if snapshot.is_some()
                && value["kind"] == "reconciliation"
                && value["status"] == "complete"
            {
                break;
            }
        }
        stop.stop();
        controller.join().unwrap().unwrap();
        assert!(saw_retry && saw_loss);
        crate::restore_snapshot(&store, &snapshot.unwrap(), temp.path().join("restored")).unwrap();
        assert_eq!(
            fs::read(temp.path().join("restored/file")).unwrap(),
            b"during capture"
        );
        assert!(!store.root().join("capture.pending").exists());
    }
    #[test]
    fn source_event_in_commit_window_is_reconciled_after_successful_capture() {
        let temp = tempfile::tempdir().unwrap();
        let store = ChunkStore::init(temp.path().join("store")).unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"before").unwrap();
        let monitor = Monitor::start_with_config(
            &store,
            &source,
            DebounceConfig::from_millis(30, 100).unwrap(),
        )
        .unwrap();
        let stop = monitor.stop_handle();
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        WORKER_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                crate::snapshot::scan::auto_tests::set_commit_hook(Box::new(move |_| {
                    blocked_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                }))
            }))
        });
        let hook = WORKER_HOOK.with(|h| h.borrow_mut().take());
        let (tx, rx) = mpsc::channel();
        struct Lines {
            bytes: Vec<u8>,
            tx: mpsc::Sender<serde_json::Value>,
        }
        impl Write for Lines {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                for l in std::mem::take(&mut self.bytes)
                    .split(|b| *b == b'\n')
                    .filter(|l| !l.is_empty())
                {
                    self.tx.send(serde_json::from_slice(l).unwrap()).unwrap();
                }
                Ok(())
            }
        }
        let controller = thread::spawn(move || {
            WORKER_HOOK.with(|h| *h.borrow_mut() = hook);
            monitor.run_automatic(&mut Lines { bytes: vec![], tx })
        });
        blocked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // This intentionally exercises the documented final-check/publication
        // TOCTOU window. The first archive is valid, but is not the latest source.
        fs::write(source.join("file"), b"after validation").unwrap();
        thread::sleep(Duration::from_millis(100));
        release_tx.send(()).unwrap();
        let mut ids = vec![];
        while ids.len() < 2 {
            let v = rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if v["kind"] == "snapshot_created" {
                ids.push(v["snapshot_id"].as_str().unwrap().to_owned());
            }
        }
        stop.stop();
        controller.join().unwrap().unwrap();
        for (i, id) in ids.iter().enumerate() {
            crate::restore_snapshot(&store, id, temp.path().join(format!("restore-{i}"))).unwrap();
        }
        assert_eq!(
            fs::read(temp.path().join("restore-0/file")).unwrap(),
            b"before"
        );
        assert_eq!(
            fs::read(temp.path().join("restore-1/file")).unwrap(),
            b"after validation"
        );
    }
}
