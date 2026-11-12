//! Diagnostic-only incremental Merkle candidates. There is intentionally no publisher.
use crate::{
    Error, Result,
    tree::{DirectoryEntry, DirectoryNode, EntryTarget},
};
use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::Metadata,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug)]
pub struct CacheLimits {
    pub entries: usize,
    pub bytes: usize,
}
impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            entries: 131_072,
            bytes: 128 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Invalidation {
    Cold,
    EventLoss,
    Ambiguous,
    Budget,
    IdentityChanged,
    AttemptFailed,
    Explicit,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DirtyScope {
    File(PathBuf),
    Directory(PathBuf),
    Subtree(PathBuf),
}
#[derive(Clone, Debug, Default)]
pub struct DirtyPlan {
    scopes: BTreeSet<DirtyScope>,
    full: Option<Invalidation>,
    bytes: usize,
}
impl DirtyPlan {
    pub fn full(reason: Invalidation) -> Self {
        Self {
            full: Some(reason),
            ..Self::default()
        }
    }
    pub fn add(&mut self, scope: DirtyScope) {
        if self.full.is_some() {
            return;
        }
        let path = match &scope {
            DirtyScope::File(p) | DirtyScope::Directory(p) | DirtyScope::Subtree(p) => p,
        };
        if path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            *self = Self::full(Invalidation::Ambiguous);
            return;
        }
        if self.scopes.contains(&scope) {
            return;
        }
        if self.scopes.len() >= 4096 || self.bytes + path.as_os_str().len() > 1024 * 1024 {
            *self = Self::full(Invalidation::Budget);
            return;
        }
        self.bytes += path.as_os_str().len();
        self.scopes.insert(scope);
    }
    pub fn merge(&mut self, other: Self) {
        if let Some(reason) = other.full {
            *self = Self::full(reason);
        } else {
            for s in other.scopes {
                self.add(s);
            }
        }
    }
    fn affects(&self, path: &Path) -> bool {
        self.scopes.iter().any(|s| match s {
            DirtyScope::File(p) | DirtyScope::Directory(p) => p.starts_with(path),
            DirtyScope::Subtree(p) => p.starts_with(path) || path.starts_with(p),
        })
    }
    fn recursive(&self, path: &Path) -> bool {
        self.scopes
            .iter()
            .any(|s| matches!(s,DirtyScope::Subtree(p) if path.starts_with(p)))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    id: (u64, u64),
    len: u64,
    links: u64,
    mode: u32,
    mtime: (i64, i64),
    ctime: (i64, i64),
}
impl Stamp {
    fn new(m: &Metadata) -> Self {
        Self {
            id: (m.dev(), m.ino()),
            len: m.len(),
            links: m.nlink(),
            mode: m.mode(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}
#[derive(Clone)]
enum Entry {
    File { stamp: Stamp, target: EntryTarget },
    Directory { stamp: Stamp, hash: String },
}
impl Entry {
    fn target(&self) -> EntryTarget {
        match self {
            Self::File { target, .. } => target.clone(),
            Self::Directory { hash, .. } => EntryTarget::Directory { tree: hash.clone() },
        }
    }
}

pub(crate) struct Seed {
    source: PathBuf,
    store: PathBuf,
    source_id: (u64, u64),
    store_id: (u64, u64),
    root: String,
    entries: BTreeMap<PathBuf, Entry>,
    aliases: BTreeMap<(u64, u64), BTreeSet<PathBuf>>,
    charged: usize,
}
pub(crate) struct SeedBuilder {
    seed: Option<Seed>,
    limits: CacheLimits,
}
impl SeedBuilder {
    pub(crate) fn new(
        source: PathBuf,
        store: PathBuf,
        source_id: (u64, u64),
        store_id: (u64, u64),
        limits: CacheLimits,
    ) -> Self {
        Self {
            seed: Some(Seed {
                source,
                store,
                source_id,
                store_id,
                root: String::new(),
                entries: BTreeMap::new(),
                aliases: BTreeMap::new(),
                charged: 0,
            }),
            limits,
        }
    }
    fn insert(&mut self, path: &Path, entry: Entry, extra: usize) {
        let Some(seed) = self.seed.as_mut() else {
            return;
        };
        let Ok(relative) = path.strip_prefix(&seed.source) else {
            self.seed = None;
            return;
        };
        // Conservative logical charge includes keys, alias index and node copies;
        // counts and payload caps bound allocation, not exact allocator/RSS bytes.
        let charge = 512 + relative.as_os_str().len() * 4 + extra;
        if seed.entries.len() >= self.limits.entries
            || seed.charged.saturating_add(charge) > self.limits.bytes
        {
            self.seed = None;
            return;
        }
        seed.charged += charge;
        if let Entry::File { stamp, .. } = &entry {
            seed.aliases
                .entry(stamp.id)
                .or_default()
                .insert(relative.to_owned());
        }
        seed.entries.insert(relative.to_owned(), entry);
    }
    pub(crate) fn file(&mut self, path: &Path, m: &Metadata, target: EntryTarget) {
        self.insert(
            path,
            Entry::File {
                stamp: Stamp::new(m),
                target,
            },
            0,
        );
    }
    pub(crate) fn directory(&mut self, path: &Path, m: &Metadata, node: &DirectoryNode) {
        if self.seed.is_none() {
            return;
        }
        let Ok(bytes) = node.canonical_bytes() else {
            self.seed = None;
            return;
        };
        if bytes.len().saturating_mul(2) > self.limits.bytes {
            self.seed = None;
            return;
        }
        self.insert(
            path,
            Entry::Directory {
                stamp: Stamp::new(m),
                hash: crate::hashing::hash(&bytes),
            },
            bytes.len() * 2,
        );
    }
    pub(crate) fn finish(mut self, root: String) -> Option<Seed> {
        if let Some(seed) = &mut self.seed {
            seed.root = root;
        }
        self.seed
    }
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct CandidateStats {
    pub files_read: u64,
    pub source_bytes: u64,
    pub directories_listed: u64,
    pub nodes_rebuilt: u64,
    pub entries_reused: u64,
}
#[derive(Debug, Serialize)]
pub struct Candidate {
    pub root_hash: String,
    pub stats: CandidateStats,
}
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ShadowResult {
    Candidate(Candidate),
    NeedsReconciliation { reason: Invalidation },
}
/// Worker-owned baseline. Candidate overlays are never installed or published.
pub struct MerkleCache {
    seed: Option<Seed>,
    limits: CacheLimits,
    invalid: Invalidation,
    epoch: u64,
}
impl Default for MerkleCache {
    fn default() -> Self {
        Self::new(CacheLimits::default())
    }
}
impl MerkleCache {
    pub fn new(limits: CacheLimits) -> Self {
        Self {
            seed: None,
            limits: CacheLimits {
                entries: limits.entries.min(131_072),
                bytes: limits.bytes.min(128 * 1024 * 1024),
            },
            invalid: Invalidation::Cold,
            epoch: 0,
        }
    }
    pub fn invalidate(&mut self, reason: Invalidation) {
        self.seed = None;
        self.invalid = reason;
        self.epoch = self.epoch.saturating_add(1);
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn charged_bytes(&self) -> usize {
        self.seed.as_ref().map_or(0, |s| s.charged)
    }
    pub fn baseline_root(&self) -> Option<&str> {
        self.seed.as_ref().map(|s| s.root.as_str())
    }
    pub fn full_scan_root(
        store: &crate::ChunkStore,
        source: &Path,
        stop: &AtomicBool,
    ) -> Result<String> {
        crate::snapshot::scan::diagnostic_seed(
            store,
            source,
            CacheLimits {
                entries: 0,
                bytes: 0,
            },
            stop,
        )
        .map(|(root, _)| root)
    }
    pub fn entry_count(&self) -> usize {
        self.seed.as_ref().map_or(0, |s| s.entries.len())
    }
    pub(crate) fn limits(&self) -> CacheLimits {
        self.limits
    }
    pub(crate) fn install(&mut self, seed: Option<Seed>) {
        self.seed = seed;
        if self.seed.is_none() {
            self.invalid = Invalidation::Budget;
        }
        self.epoch = self.epoch.saturating_add(1);
    }
    /// Diagnostic warmup is read-only and not a validated/committed baseline.
    /// The automatic worker instead installs the seed from a successful C attempt.
    pub fn diagnostic_baseline(
        &mut self,
        store: &crate::ChunkStore,
        source: &Path,
        stop: &AtomicBool,
    ) -> Result<String> {
        let (root, seed) =
            crate::snapshot::scan::diagnostic_seed(store, source, self.limits, stop)?;
        self.install(seed);
        Ok(root)
    }
    pub fn candidate(&mut self, plan: &DirtyPlan, stop: &AtomicBool) -> Result<ShadowResult> {
        let result = if stop.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            self.build_candidate(plan, stop)
        };
        if result.is_err() {
            self.invalidate(Invalidation::AttemptFailed);
        }
        result
    }
    fn build_candidate(&mut self, plan: &DirtyPlan, stop: &AtomicBool) -> Result<ShadowResult> {
        if let Some(reason) = plan.full {
            self.invalidate(reason);
            return Ok(ShadowResult::NeedsReconciliation { reason });
        }
        let Some(seed) = &self.seed else {
            return Ok(ShadowResult::NeedsReconciliation {
                reason: self.invalid,
            });
        };
        let mut replaced = false;
        for (path, id) in [(&seed.source, seed.source_id), (&seed.store, seed.store_id)] {
            let m = std::fs::symlink_metadata(path).map_err(Error::SourceIo)?;
            if !m.is_dir() || (m.dev(), m.ino()) != id {
                replaced = true;
                break;
            }
        }
        if replaced {
            self.invalidate(Invalidation::IdentityChanged);
            return Ok(ShadowResult::NeedsReconciliation {
                reason: Invalidation::IdentityChanged,
            });
        }
        let mut expanded = plan.clone();
        for scope in &plan.scopes {
            let p = match scope {
                DirtyScope::File(p) | DirtyScope::Directory(p) | DirtyScope::Subtree(p) => p,
            };
            if let Some(Entry::File { stamp, .. }) = seed.entries.get(p) {
                let aliases = &seed.aliases[&stamp.id];
                if stamp.links > aliases.len() as u64 {
                    return Ok(ShadowResult::NeedsReconciliation {
                        reason: Invalidation::Ambiguous,
                    });
                }
                for alias in aliases {
                    expanded.add(DirtyScope::File(alias.clone()));
                }
            }
        }
        if let Some(reason) = expanded.full {
            return Ok(ShadowResult::NeedsReconciliation { reason });
        }
        let root = open_dir(rustix::fs::CWD, &seed.source)?;
        let root_meta = root.metadata().map_err(Error::SourceIo)?;
        if (root_meta.dev(), root_meta.ino()) != seed.source_id {
            return Ok(ShadowResult::NeedsReconciliation {
                reason: Invalidation::IdentityChanged,
            });
        }
        let mut builder = Builder {
            seed,
            plan: &expanded,
            stop,
            stats: CandidateStats::default(),
            work: 0,
            limits: self.limits,
        };
        let hash = builder.directory(&root, Path::new(""), 0, false)?;
        Ok(ShadowResult::Candidate(Candidate {
            root_hash: hash,
            stats: builder.stats,
        }))
    }
}
fn open_dir(parent: impl std::os::fd::AsFd, path: &Path) -> Result<std::fs::File> {
    use rustix::fs::{self as r, Mode, OFlags};
    Ok(r::openat(
        parent,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| Error::SourceIo(e.into()))?
    .into())
}
struct Builder<'a> {
    seed: &'a Seed,
    plan: &'a DirtyPlan,
    stop: &'a AtomicBool,
    stats: CandidateStats,
    work: usize,
    limits: CacheLimits,
}
impl Builder<'_> {
    fn check(&mut self, bytes: usize) -> Result<()> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.work = self.work.saturating_add(bytes);
        if self.work > self.limits.bytes {
            return Err(Error::Invalid(
                "shadow candidate work budget exceeded".into(),
            ));
        }
        Ok(())
    }
    fn directory(
        &mut self,
        dir: &std::fs::File,
        path: &Path,
        depth: usize,
        recursive: bool,
    ) -> Result<String> {
        use rustix::fs::{self as r, AtFlags, Dir, FileType};
        self.check(512)?;
        if depth > 256 {
            return Err(Error::Invalid("directory depth exceeds 256".into()));
        }
        self.stats.directories_listed += 1;
        let before = Stamp::new(&dir.metadata().map_err(Error::SourceIo)?);
        let mut names = Vec::new();
        for entry in Dir::read_from(dir).map_err(|e| Error::SourceIo(e.into()))? {
            let entry = entry.map_err(|e| Error::SourceIo(e.into()))?;
            let name = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            let name = name
                .to_str()
                .map_err(|_| Error::Invalid("non-UTF-8 source entry".into()))?;
            self.check(512 + name.len() * 4)?;
            names.push(name.to_owned());
        }
        names.sort();
        let mut entries = Vec::with_capacity(names.len());
        let recursive = recursive || self.plan.recursive(path);
        for name in names {
            let child_path = path.join(&name);
            let stat = r::statat(dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|e| Error::SourceIo(e.into()))?;
            let old = self.seed.entries.get(&child_path);
            let target = match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    let child = open_dir(dir, Path::new(&name))?;
                    let m = child.metadata().map_err(Error::SourceIo)?;
                    if (m.dev(), m.ino()) == self.seed.store_id {
                        continue;
                    }
                    crate::tree::validate_name(&name)?;
                    let stamp = Stamp::new(&m);
                    if !recursive
                        && !self.plan.affects(&child_path)
                        && matches!(old,Some(Entry::Directory{stamp:s,..})if *s==stamp)
                    {
                        self.stats.entries_reused += 1;
                        old.unwrap().target()
                    } else {
                        EntryTarget::Directory {
                            tree: self.directory(
                                &child,
                                &child_path,
                                depth + 1,
                                recursive || !matches!(old, Some(Entry::Directory { .. })),
                            )?,
                        }
                    }
                }
                FileType::RegularFile => {
                    crate::tree::validate_name(&name)?;
                    self.file(dir, &name, &child_path, old, recursive)?
                }
                _ => {
                    return Err(Error::Invalid(format!(
                        "unsupported shadow source entry: {}",
                        child_path.display()
                    )));
                }
            };
            entries.push(DirectoryEntry::new(name, target)?);
        }
        if before != Stamp::new(&dir.metadata().map_err(Error::SourceIo)?) {
            return Err(Error::Unstable(path.display().to_string()));
        }
        self.stats.nodes_rebuilt += 1;
        DirectoryNode::new(entries)?.hash()
    }
    fn file(
        &mut self,
        dir: &std::fs::File,
        name: &str,
        path: &Path,
        old: Option<&Entry>,
        recursive: bool,
    ) -> Result<EntryTarget> {
        use rustix::fs::{self as r, Mode, OFlags};
        self.check(128)?;
        let file: std::fs::File = r::openat(
            dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| Error::SourceIo(e.into()))?
        .into();
        let m = file.metadata().map_err(Error::SourceIo)?;
        if !m.is_file() {
            return Err(Error::Unstable(path.display().to_string()));
        }
        let before = Stamp::new(&m);
        if !recursive
            && !self.plan.affects(path)
            && matches!(old,Some(Entry::File{stamp,..})if *stamp==before)
        {
            self.stats.entries_reused += 1;
            return Ok(old.unwrap().target());
        }
        self.stats.files_read += 1;
        self.stats.source_bytes += m.len();
        let manifest = crate::manifest::ingest(
            &file,
            crate::chunking::ChunkSizes::default(),
            true,
            |bytes| {
                // Bound per-file manifest construction as well as cached entries.
                self.check(256)?;
                Ok(crate::hashing::hash(bytes))
            },
        )?;
        if before != Stamp::new(&file.metadata().map_err(Error::SourceIo)?) {
            return Err(Error::Unstable(path.display().to_string()));
        }
        Ok(EntryTarget::File {
            manifest: crate::hashing::hash(&manifest.encoded_bytes()?),
            length: manifest.length,
        })
    }
}
