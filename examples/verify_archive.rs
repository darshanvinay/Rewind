//! Standalone benchmark of a complete archive verification pass, without source reads.
//! This is an example/measurement tool, not a new CLI command.
use rewind::{
    ChunkStore, FileManifest, Result,
    snapshot::SnapshotRecord,
    tree::{DirectoryNode, EntryTarget},
};
use std::time::Instant;
fn walk(
    store: &ChunkStore,
    root: &str,
    depth: usize,
    files: &mut u64,
    bytes: &mut u64,
) -> Result<()> {
    if depth > 256 {
        return Err(rewind::Error::Invalid("archive depth limit".into()));
    }
    for entry in DirectoryNode::load(store, root)?.entries() {
        match entry.target() {
            EntryTarget::Directory { tree } => walk(store, tree, depth + 1, files, bytes)?,
            EntryTarget::File { manifest, length } => {
                let recipe = FileManifest::load_by_hash(store, manifest)?;
                if recipe.length != *length {
                    return Err(rewind::Error::Corrupt(manifest.clone()));
                }
                rewind::verify(store, &recipe)?;
                *files += 1;
                *bytes += length;
            }
        }
    }
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err(rewind::Error::Invalid(
            "verify_archive <store> <snapshot-id>".into(),
        ));
    }
    let store = ChunkStore::open(&args[1])?;
    let record = SnapshotRecord::load(&store, &args[2])?;
    let mut files = 0;
    let mut bytes = 0;
    let start = Instant::now();
    walk(&store, record.root_hash(), 0, &mut files, &mut bytes)?;
    println!(
        "{}",
        serde_json::json!({"archive_verification_ms":start.elapsed().as_secs_f64()*1000.0,"files":files,"logical_bytes":bytes})
    );
    Ok(())
}
