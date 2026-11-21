use std::process::Command;

#[test]
fn help_and_version_work_without_a_store() {
    let temp = tempfile::tempdir().unwrap();
    for flag in ["--help", "-h", "--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_rewind"))
            .arg(flag)
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            String::from_utf8(out.stdout)
                .unwrap()
                .contains(env!("CARGO_PKG_VERSION"))
        );
    }
    assert!(!temp.path().join(".rewind").exists());
}

#[test]
fn unfinished_fast_and_invalid_configuration_fail_before_storage_mutations() {
    let temp = tempfile::tempdir().unwrap();
    for options in [
        vec!["--mode", "fast"],
        vec!["--mode", "unknown"],
        vec!["--mode", "strict", "--mode", "strict"],
        vec!["--reconcile-interval-secs", "0"],
        vec!["--reconcile-interval-secs", "86401"],
        vec!["--reconcile-interval-secs"],
        vec!["--debounce-ms", "0"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_rewind"))
            .args(["watch", "missing"])
            .args(options)
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(!out.status.success());
        let error: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(error["fatal"], true);
        assert_eq!(error["code"], "watch_setup");
        assert!(!temp.path().join(".rewind").exists());
    }
}
