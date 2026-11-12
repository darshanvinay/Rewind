//! Bounded, clock-driven accumulation of event hints. No filesystem/storage I/O.
use crate::{Error, Result, watch::NormalizedEvent};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

const MAX_PATHS: usize = 4096;
const MAX_RENAMES: usize = 1024;
const MAX_BYTES: usize = 1024 * 1024;

/// Quiet period and absolute collection deadline. Neither implies file stability.
#[derive(Clone, Copy, Debug)]
pub struct DebounceConfig {
    quiet: Duration,
    max_delay: Duration,
}
impl Default for DebounceConfig {
    fn default() -> Self {
        Self {
            quiet: Duration::from_millis(300),
            max_delay: Duration::from_secs(2),
        }
    }
}
impl DebounceConfig {
    pub fn from_millis(quiet: u64, max_delay: u64) -> Result<Self> {
        if quiet == 0 || quiet > max_delay || max_delay > 60_000 {
            return Err(Error::Invalid(
                "watch timing requires 1 <= debounce-ms <= max-delay-ms <= 60000".into(),
            ));
        }
        Ok(Self {
            quiet: Duration::from_millis(quiet),
            max_delay: Duration::from_millis(max_delay),
        })
    }
    pub fn debounce_ms(self) -> u64 {
        self.quiet.as_millis() as u64
    }
    pub fn max_delay_ms(self) -> u64 {
        self.max_delay.as_millis() as u64
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct Change {
    path: String,
    kinds: BTreeSet<&'static str>,
}

/// Keep only observed pairings; never join separate rename halves by proximity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) struct RenameHint {
    kind: &'static str,
    paths: Vec<String>,
    tracker: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Batch {
    kind: &'static str,
    changes: Vec<Change>,
    renames: BTreeSet<RenameHint>,
    pub reconciliation_required: bool,
    reconciliation_reasons: BTreeSet<&'static str>,
}
#[cfg(unix)]
impl Batch {
    pub(crate) fn dirty_plan(&self) -> crate::shadow::DirtyPlan {
        use crate::shadow::{DirtyPlan, DirtyScope, Invalidation};
        if self
            .reconciliation_reasons
            .iter()
            .any(|r| *r != "rename_scope")
        {
            return DirtyPlan::full(Invalidation::EventLoss);
        }
        let mut plan = DirtyPlan::default();
        for change in &self.changes {
            plan.add(DirtyScope::File(change.path.clone().into()));
        }
        for rename in &self.renames {
            if rename.kind != "rename" || rename.paths.len() != 2 {
                return DirtyPlan::full(Invalidation::Ambiguous);
            }
            for path in &rename.paths {
                plan.add(DirtyScope::Subtree(path.into()));
            }
        }
        plan
    }
}

struct Pending {
    first: Instant,
    last: Instant,
    paths: BTreeMap<String, BTreeSet<&'static str>>,
    renames: BTreeSet<RenameHint>,
    bytes: usize,
}

pub(crate) struct Debouncer {
    config: DebounceConfig,
    pending: Option<Pending>,
    // Sticky for this monitoring session: B has no authoritative reconciliation
    // or acknowledgement. C must clear this only after a successful full scan.
    reasons: BTreeSet<&'static str>,
}
impl Debouncer {
    pub fn new(config: DebounceConfig) -> Self {
        Self {
            config,
            pending: None,
            reasons: BTreeSet::new(),
        }
    }
    fn pending(&mut self, now: Instant) -> &mut Pending {
        self.pending.get_or_insert_with(|| Pending {
            first: now,
            last: now,
            paths: BTreeMap::new(),
            renames: BTreeSet::new(),
            bytes: 0,
        })
    }
    pub fn reconcile(&mut self, reason: &'static str, now: Instant) {
        self.reasons.insert(reason);
        self.pending(now).last = now;
    }
    pub fn push(&mut self, event: NormalizedEvent, now: Instant) {
        if let Some(reason) = event.reconciliation_reason {
            self.reconcile(reason, now);
        }
        if event.kind == "reconcile" {
            return;
        }
        if matches!(event.kind, "any" | "other") || event.paths.iter().any(|p| p == ".") {
            self.reasons.insert("coarse_event");
        }
        if event.kind.starts_with("rename") {
            // A rename may describe a directory subtree or an unpaired half.
            self.reasons.insert("rename_scope");
        }
        let pending = self.pending(now);
        pending.last = now;
        let mut capped = false;
        for path in &event.paths {
            if let Some(kinds) = pending.paths.get_mut(path) {
                kinds.insert(event.kind);
            } else if pending.paths.len() < MAX_PATHS && pending.bytes + path.len() <= MAX_BYTES {
                pending.bytes += path.len();
                pending
                    .paths
                    .insert(path.clone(), BTreeSet::from([event.kind]));
            } else {
                capped = true;
            }
        }
        if event.kind.starts_with("rename") {
            let hint = RenameHint {
                kind: event.kind,
                paths: event.paths,
                tracker: event.tracker,
            };
            let bytes: usize = hint.paths.iter().map(String::len).sum();
            if !pending.renames.contains(&hint) {
                if pending.renames.len() < MAX_RENAMES && pending.bytes + bytes <= MAX_BYTES {
                    pending.bytes += bytes;
                    pending.renames.insert(hint);
                } else {
                    capped = true;
                }
            }
        }
        if capped {
            self.reasons.insert("batch_capacity");
        }
    }
    pub(crate) fn acknowledge_reconciliation(&mut self) {
        self.reasons.clear();
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending
            .as_ref()
            .map(|p| (p.first + self.config.max_delay).min(p.last + self.config.quiet))
    }
    pub fn take(&mut self) -> Option<Batch> {
        let pending = self.pending.take()?;
        Some(Batch {
            kind: "batch",
            changes: pending
                .paths
                .into_iter()
                .map(|(path, kinds)| Change { path, kinds })
                .collect(),
            renames: pending.renames,
            reconciliation_required: !self.reasons.is_empty(),
            reconciliation_reasons: self.reasons.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(kind: &'static str, paths: &[&str]) -> NormalizedEvent {
        NormalizedEvent {
            kind,
            paths: paths.iter().map(|s| (*s).into()).collect(),
            tracker: None,
            reconciliation_reason: None,
        }
    }
    #[test]
    fn quiet_period_moves_but_absolute_deadline_does_not() {
        let now = Instant::now();
        let mut d = Debouncer::new(DebounceConfig::default());
        assert!(d.deadline().is_none());
        d.push(event("modify", &["file"]), now);
        assert_eq!(d.deadline(), Some(now + Duration::from_millis(300)));
        for ms in (200..=1800).step_by(200) {
            d.push(event("modify", &["file"]), now + Duration::from_millis(ms));
        }
        assert_eq!(d.deadline(), Some(now + Duration::from_secs(2)));
        let b = d.take().unwrap();
        assert_eq!(b.changes.len(), 1);
        assert_eq!(b.changes[0].kinds, BTreeSet::from(["modify"]));
        assert!(!b.reconciliation_required);
        assert!(d.deadline().is_none());
        d.push(event("modify", &["next"]), now + Duration::from_secs(3));
        assert_eq!(d.take().unwrap().changes[0].path, "next");
    }
    #[test]
    fn create_delete_and_atomic_save_keep_all_paths_and_observed_rename_order() {
        let mut d = Debouncer::new(DebounceConfig::default());
        let now = Instant::now();
        d.push(event("create", &["temp"]), now);
        d.push(event("modify", &["temp"]), now);
        let mut rename = event("rename", &["temp", "target"]);
        rename.tracker = Some(17);
        d.push(rename, now);
        d.push(event("remove", &["temp"]), now);
        d.push(event("rename-from", &["dir-old"]), now);
        d.push(event("rename-to", &["dir-new"]), now);
        let b = d.take().unwrap();
        assert_eq!(
            b.changes
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            ["dir-new", "dir-old", "target", "temp"]
        );
        assert_eq!(
            b.changes.last().unwrap().kinds,
            BTreeSet::from(["create", "modify", "remove", "rename"])
        );
        assert!(
            b.renames
                .iter()
                .any(|r| r.paths == ["temp", "target"] && r.tracker == Some(17))
        );
        assert_eq!(b.renames.len(), 3); // Do not guess pairs from adjacency.
        assert!(b.reconciliation_required);
    }
    #[test]
    fn reconciliation_is_sticky_without_creating_idle_heartbeat_batches() {
        let mut d = Debouncer::new(DebounceConfig::default());
        let now = Instant::now();
        d.reconcile("backend_rescan", now);
        let b = d.take().unwrap();
        assert!(b.reconciliation_required);
        assert!(b.changes.is_empty());
        assert!(d.take().is_none());
        d.push(event("modify", &["later"]), now);
        assert!(
            d.take()
                .unwrap()
                .reconciliation_reasons
                .contains("backend_rescan")
        );
    }
    #[test]
    fn bounds_collapse_excess_hints_into_reconciliation_without_unbounded_growth() {
        let now = Instant::now();
        let mut d = Debouncer::new(DebounceConfig::default());
        for i in 0..MAX_PATHS + 10 {
            d.push(event("create", &[&format!("file-{i}")]), now);
        }
        let b = d.take().unwrap();
        assert_eq!(b.changes.len(), MAX_PATHS);
        assert!(b.reconciliation_reasons.contains("batch_capacity"));
        let mut d = Debouncer::new(DebounceConfig::default());
        for i in 0..MAX_RENAMES + 10 {
            d.push(
                event("rename", &[&format!("from-{i}"), &format!("to-{i}")]),
                now,
            );
        }
        let b = d.take().unwrap();
        assert_eq!(b.renames.len(), MAX_RENAMES);
        assert!(b.reconciliation_reasons.contains("batch_capacity"));
        let mut d = Debouncer::new(DebounceConfig::default());
        for i in 0..32 {
            d.push(
                event("modify", &[&format!("{i}{}", "x".repeat(65536))]),
                now,
            );
        }
        assert!(d.pending.as_ref().unwrap().bytes <= MAX_BYTES);
        assert!(d.take().unwrap().reconciliation_required);
    }
    #[test]
    fn timing_configuration_is_bounded_and_consistent() {
        for (quiet, max) in [(0, 2000), (301, 300), (1, 60001), (u64::MAX, u64::MAX)] {
            assert!(DebounceConfig::from_millis(quiet, max).is_err());
        }
        assert!(DebounceConfig::from_millis(1, 1).is_ok());
        assert!(DebounceConfig::from_millis(60000, 60000).is_ok());
    }
}
