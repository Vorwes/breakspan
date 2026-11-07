//! Exercise the actual executable without transport types outside OTLP.
mod common;

use common::{PRIVATE, TempDir, fixture};
use std::{fs, process::Command};

const TREE: &str = "Trace 01010101010101010101010101010101 (snapshot, 3 spans)\n\
synthetic agent  4.21s\n\
├── synthetic model replacement  6ms\n\
└── synthetic tool  6ms [error]\n\n";

#[test]
fn show_prints_final_fixture_tree_once() {
    let output = Command::new(env!("CARGO_BIN_EXE_breakspan"))
        .arg("show")
        .arg(fixture())
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), TREE);
    assert!(output.stderr.is_empty());
}

#[test]
fn show_retains_valid_prefix_but_signals_corruption_to_scripts() {
    let temp = TempDir::new();
    let path = temp.0.join(format!("{PRIVATE}.bspan"));
    let mut bytes = fs::read(fixture()).unwrap();
    bytes.push(0xff); // The next frame's length prefix is truncated.
    fs::write(&path, bytes).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_breakspan"))
        .arg("show")
        .arg(path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), TREE);
    assert!(!String::from_utf8(output.stderr).unwrap().contains(PRIVATE));
}

#[test]
fn show_refuses_invalid_headers_without_echoing_file_or_payload() {
    let temp = TempDir::new();
    let path = temp.0.join(format!("{PRIVATE}.bspan"));
    fs::write(&path, PRIVATE).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_breakspan"))
        .arg("show")
        .arg(path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Error: recording: bad magic\n"
    );
}

#[test]
fn listen_refuses_existing_recording_with_readable_error() {
    let temp = TempDir::new();
    let path = temp.0.join(format!("{PRIVATE}.bspan"));
    fs::write(&path, PRIVATE).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_breakspan"))
        .args(["listen", "--bind", "127.0.0.1:0", "--record"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.ends_with("Error: recording already exists; use --force to overwrite\n"),
        "{stderr}"
    );
    assert!(!stderr.contains(PRIVATE));
    assert_eq!(fs::read_to_string(&path).unwrap(), PRIVATE);
}
