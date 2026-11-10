use rewind::{ChunkStore, FileManifest, Result, chunking::ChunkSizes};
use std::{ffi::OsString, io::Write, path::PathBuf, process::ExitCode};

fn run(args: &[OsString], output: &mut impl Write) -> Result<()> {
    match args {
        [command] if command == "init" => {
            ChunkStore::init(".rewind")?;
            writeln!(output, "Initialized .rewind")?;
        }
        [command, file] if command == "store" => {
            let store = ChunkStore::open(".rewind")?;
            let manifest = rewind::store_file(&store, file, ChunkSizes::default())?;
            writeln!(output, "{}", manifest.save(&store)?.display())?;
        }
        [command, arguments @ ..] if command == "watch" => {
            let setup = (|| -> Result<rewind::watch::Monitor> {
                let (directory, options) = arguments.split_first().ok_or_else(|| rewind::Error::Invalid("watch requires a directory".into()))?;
                let config = watch_options(options)?;
                let store = ChunkStore::open(".rewind")?;
                let monitor = rewind::watch::Monitor::start_with_config(&store, directory, config)?;
                let stop = monitor.stop_handle();
                ctrlc::set_handler(move || stop.stop())
                    .map_err(|e| rewind::Error::Invalid(format!("cannot install Ctrl+C handler: {e}")))?;
                Ok(monitor)
            })();
            match setup {
                Ok(monitor) => monitor.run_automatic(output)?,
                Err(error) => {
                    writeln!(output,"{}",serde_json::json!({"kind":"error","fatal":true,"code":"watch_setup","message":error.to_string()}))?;
                    output.flush()?;
                    return Err(error);
                }
            }
        }
        [command, directory] if command == "snapshot" => {
            let store = ChunkStore::open(".rewind")?;
            let snapshot = rewind::snapshot::create_snapshot(&store, directory)?;
            writeln!(output, "{}", snapshot.id())?;
        }
        [command] if command == "snapshots" => {
            let store = ChunkStore::open(".rewind")?;
            let records = rewind::snapshot::list_snapshots(&store)?;
            writeln!(output, "ID\tUNIX_TIMESTAMP\tSOURCE")?;
            for record in records {
                let (seconds, nanos) = record.created_at();
                writeln!(output, "{}\t{seconds}.{nanos:09}\t{:?}", record.id(), record.source_path())?;
            }
        }
        [command, id, destination] if command == "restore-snapshot" => {
            let store = ChunkStore::open(".rewind")?;
            let id = id.to_str().ok_or_else(|| rewind::Error::Invalid("snapshot ID must be UTF-8".into()))?;
            rewind::restore_snapshot(&store, id, destination)?;
        }
        [command, before, after] if command == "diff" => {
            use rewind::diff::{ChangeKind, EntryKind};
            let store = ChunkStore::open(".rewind")?;
            let before = before.to_str().ok_or_else(|| rewind::Error::Invalid("snapshot ID must be UTF-8".into()))?;
            let after = after.to_str().ok_or_else(|| rewind::Error::Invalid("snapshot ID must be UTF-8".into()))?;
            let changes = rewind::diff_snapshots(&store, before, after)?;
            for change in changes {
                let status = match change.kind {
                    ChangeKind::Added => "A",
                    ChangeKind::Deleted => "D",
                    ChangeKind::Modified => "M",
                };
                let kind = match change.entry_kind {
                    EntryKind::File => "file",
                    EntryKind::Directory => "directory",
                };
                writeln!(output, "{status}\t{kind}\t{:?}", change.path)?;
            }
        }
        [command, path] if command == "verify" => {
            let store = ChunkStore::open(".rewind")?;
            rewind::verify(&store, &FileManifest::load(PathBuf::from(path))?)?;
            writeln!(output, "Verified")?;
        }
        [command, path, destination] if command == "restore" => {
            let store = ChunkStore::open(".rewind")?;
            rewind::restore_file(&store, &FileManifest::load(PathBuf::from(path))?, destination)?;
        }
        _ => return Err(rewind::Error::Invalid(
            "usage: rewind init | store <file> | restore <manifest> <output> | verify <manifest> | snapshot <directory> | watch <directory> [--debounce-ms <n>] [--max-delay-ms <n>] | snapshots | restore-snapshot <snapshot-id> <output-directory> | diff <snapshot-a> <snapshot-b>"
                .into(),
        )),
    }
    Ok(())
}
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mut output = std::io::stdout().lock();
    let result = run(&args, &mut output).and_then(|()| output.flush().map_err(Into::into));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rewind: {e}");
            ExitCode::FAILURE
        }
    }
}

fn watch_options(options: &[OsString]) -> Result<rewind::watch::DebounceConfig> {
    let mut quiet = None;
    let mut maximum = None;
    let (pairs, remainder) = options.as_chunks::<2>();
    for pair in pairs {
        let value = pair[1]
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| {
                rewind::Error::Invalid("watch timing must be integer milliseconds".into())
            })?;
        let target = if pair[0] == "--debounce-ms" {
            &mut quiet
        } else if pair[0] == "--max-delay-ms" {
            &mut maximum
        } else {
            return Err(rewind::Error::Invalid("unknown watch option".into()));
        };
        if target.replace(value).is_some() {
            return Err(rewind::Error::Invalid("duplicate watch option".into()));
        }
    }
    if !remainder.is_empty() {
        return Err(rewind::Error::Invalid(
            "watch option requires a value".into(),
        ));
    }
    rewind::watch::DebounceConfig::from_millis(quiet.unwrap_or(300), maximum.unwrap_or(2000))
}
