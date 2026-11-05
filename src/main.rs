use rewind::{ChunkStore, FileManifest, Result, chunking::ChunkSizes};
use std::{ffi::OsString, path::PathBuf, process::ExitCode};

fn run(args: &[OsString]) -> Result<()> {
    match args {
        [command] if command == "init" => {
            ChunkStore::init(".rewind")?;
            println!("Initialized .rewind");
        }
        [command, file] if command == "store" => {
            let store = ChunkStore::open(".rewind")?;
            let manifest = rewind::store_file(&store, file, ChunkSizes::default())?;
            println!("{}", manifest.save(&store)?.display());
        }
        [command, directory] if command == "snapshot" => {
            let store = ChunkStore::open(".rewind")?;
            let snapshot = rewind::snapshot::create_snapshot(&store, directory)?;
            println!("{}", snapshot.id());
        }
        [command, path] if command == "verify" => {
            let store = ChunkStore::open(".rewind")?;
            rewind::verify(&store, &FileManifest::load(PathBuf::from(path))?)?;
            println!("Verified");
        }
        [command, path, output] if command == "restore" => {
            let store = ChunkStore::open(".rewind")?;
            rewind::restore_file(&store, &FileManifest::load(PathBuf::from(path))?, output)?;
        }
        _ => return Err(rewind::Error::Invalid(
            "usage: rewind init | store <file> | restore <manifest> <output> | verify <manifest> | snapshot <directory>"
                .into(),
        )),
    }
    Ok(())
}
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rewind: {e}");
            ExitCode::FAILURE
        }
    }
}
