//! Relative-path content diffs that prune equal directory identities.
use crate::{
    ChunkStore, Error, FileManifest, Result,
    snapshot::SnapshotRecord,
    tree::{DirectoryEntry, DirectoryNode, EntryTarget},
};
use std::cmp::Ordering;

const MAX_DEPTH: usize = 256;

/// An addition, deletion, or byte-content modification relative to the first root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
}

/// The affected object's type. Directories are only added or deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// A path relative to the snapshot root, using '/' separators on every platform.
/// A type replacement has two entries: deletion of the old type, then addition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
    pub entry_kind: EntryKind,
}

/// Compare snapshots by relative hierarchy and file bytes, ignoring capture
/// timestamps/source locations. Verify root nodes and all metadata read, skipping
/// equal directory subtrees and equal file references. No chunks are read: this
/// reports content differences, not the integrity/restorability of both archives.
/// Results are sorted by UTF-8 path bytes, with deletion before addition at the
/// same path. Renames are represented as deletion/addition. Depth limit is 256.
pub fn diff_snapshots(store: &ChunkStore, before: &str, after: &str) -> Result<Vec<Change>> {
    let before = SnapshotRecord::load(store, before)?;
    let after = SnapshotRecord::load(store, after)?;
    // Always validate a root, including an equal-root comparison. Descendants
    // can be skipped by identity; the explicitly requested roots must exist.
    let old = DirectoryNode::load(store, before.root_hash())?;
    if before.root_hash() == after.root_hash() {
        return Ok(Vec::new());
    }
    let new = DirectoryNode::load(store, after.root_hash())?;
    let mut changes = Vec::new();
    compare_nodes(store, &old, &new, "", 0, &mut changes)?;
    changes.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| order(a.kind).cmp(&order(b.kind)))
    });
    Ok(changes)
}

fn order(kind: ChangeKind) -> u8 {
    match kind {
        ChangeKind::Deleted => 0,
        ChangeKind::Added => 1,
        ChangeKind::Modified => 2,
    }
}

fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn check_depth(depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::Invalid("diff directory depth exceeds 256".into()));
    }
    Ok(())
}

fn compare_nodes(
    store: &ChunkStore,
    old: &DirectoryNode,
    new: &DirectoryNode,
    prefix: &str,
    depth: usize,
    changes: &mut Vec<Change>,
) -> Result<()> {
    check_depth(depth)?;
    let (mut i, mut j) = (0, 0);
    loop {
        match (old.entries().get(i), new.entries().get(j)) {
            (Some(a), Some(b)) => match a.name().cmp(b.name()) {
                Ordering::Less => {
                    expand(
                        store,
                        a.target(),
                        &child_path(prefix, a.name()),
                        depth + 1,
                        ChangeKind::Deleted,
                        changes,
                    )?;
                    i += 1;
                }
                Ordering::Greater => {
                    expand(
                        store,
                        b.target(),
                        &child_path(prefix, b.name()),
                        depth + 1,
                        ChangeKind::Added,
                        changes,
                    )?;
                    j += 1;
                }
                Ordering::Equal => {
                    compare_entries(
                        store,
                        a,
                        b,
                        &child_path(prefix, a.name()),
                        depth + 1,
                        changes,
                    )?;
                    i += 1;
                    j += 1;
                }
            },
            (Some(a), None) => {
                expand(
                    store,
                    a.target(),
                    &child_path(prefix, a.name()),
                    depth + 1,
                    ChangeKind::Deleted,
                    changes,
                )?;
                i += 1;
            }
            (None, Some(b)) => {
                expand(
                    store,
                    b.target(),
                    &child_path(prefix, b.name()),
                    depth + 1,
                    ChangeKind::Added,
                    changes,
                )?;
                j += 1;
            }
            (None, None) => return Ok(()),
        }
    }
}

fn compare_entries(
    store: &ChunkStore,
    old: &DirectoryEntry,
    new: &DirectoryEntry,
    path: &str,
    depth: usize,
    changes: &mut Vec<Change>,
) -> Result<()> {
    match (old.target(), new.target()) {
        (EntryTarget::Directory { tree: a }, EntryTarget::Directory { tree: b }) => {
            if a == b {
                return Ok(());
            }
            check_depth(depth)?;
            let a = DirectoryNode::load(store, a)?;
            let b = DirectoryNode::load(store, b)?;
            compare_nodes(store, &a, &b, path, depth, changes)
        }
        (
            EntryTarget::File {
                manifest: a,
                length: a_len,
            },
            EntryTarget::File {
                manifest: b,
                length: b_len,
            },
        ) => {
            if a == b && a_len == b_len {
                return Ok(());
            }
            let a = file_identity(store, a, *a_len)?;
            let b = file_identity(store, b, *b_len)?;
            if a != b {
                changes.push(Change {
                    path: path.into(),
                    kind: ChangeKind::Modified,
                    entry_kind: EntryKind::File,
                });
            }
            Ok(())
        }
        (a, b) => {
            expand(store, a, path, depth, ChangeKind::Deleted, changes)?;
            expand(store, b, path, depth, ChangeKind::Added, changes)
        }
    }
}

fn file_identity(store: &ChunkStore, hash: &str, length: u64) -> Result<(String, u64)> {
    let manifest = FileManifest::load_by_hash(store, hash)?;
    if manifest.length != length {
        return Err(Error::Corrupt(format!(
            "file length mismatch in manifest {hash}"
        )));
    }
    Ok((manifest.file_hash, manifest.length))
}

fn expand(
    store: &ChunkStore,
    target: &EntryTarget,
    path: &str,
    depth: usize,
    kind: ChangeKind,
    changes: &mut Vec<Change>,
) -> Result<()> {
    match target {
        EntryTarget::File { manifest, length } => {
            file_identity(store, manifest, *length)?;
            changes.push(Change {
                path: path.into(),
                kind,
                entry_kind: EntryKind::File,
            });
        }
        EntryTarget::Directory { tree } => {
            check_depth(depth)?;
            let node = DirectoryNode::load(store, tree)?;
            changes.push(Change {
                path: path.into(),
                kind,
                entry_kind: EntryKind::Directory,
            });
            for child in node.entries() {
                expand(
                    store,
                    child.target(),
                    &child_path(path, child.name()),
                    depth + 1,
                    kind,
                    changes,
                )?;
            }
        }
    }
    Ok(())
}
