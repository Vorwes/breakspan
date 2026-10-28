//! Exercise the actual executable without transport types outside OTLP.
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

const TREE: &str = "Trace 01010101010101010101010101010101 (snapshot, 3 spans)\n\
synthetic agent  4.21s\n\
├── synthetic model replacement  6ms\n\
└── synthetic tool  6ms [error]\n\n";
const PRIVATE: &str = "PRIVATE_CAPTURE_CONTENT";

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../breakspan-otlp/tests/fixtures/synthetic.bspan")
}

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "breakspan-show-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test directory: {error}"),
            }
        }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

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
    assert!(!String::from_utf8(output.stderr).unwrap().contains(PRIVATE));
}
