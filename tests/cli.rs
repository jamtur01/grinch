use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn cli_reports_bad_configs_and_arguments() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let home: PathBuf = std::env::temp_dir().join(format!(
        "grinch-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".grinch.js"), "module.exports = {};").unwrap();
    let binary = env!("CARGO_BIN_EXE_Grinch");
    for args in [
        vec!["--test", "https://example.com"],
        vec!["--list-rules"],
        vec!["--bench", "1", "https://example.com"],
        vec!["--bench-init", "1"],
        vec!["--validate"],
    ] {
        let output = Command::new(binary)
            .env("HOME", &home)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
    }
    for args in [
        vec!["--bench", "abc", "https://example.com"],
        vec!["--bench-init", "abc"],
    ] {
        let output = Command::new(binary)
            .env("HOME", &home)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
    }
    let output = Command::new(binary)
        .env("HOME", &home)
        .arg("--test")
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("valid UTF-8"));
    std::fs::remove_dir_all(home).unwrap();
}
