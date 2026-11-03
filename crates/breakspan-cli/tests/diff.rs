//! Exercise `breakspan diff` through the executable and real recordings.
#![allow(
    clippy::unwrap_used,
    reason = "test helpers panic on setup failure; clippy's test exemption covers only #[test] functions"
)]
mod common;

use common::{PRIVATE, TempDir, fixture};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const HEADER_LEN: usize = 8;

/// The fixture's frames: [tool + superseded model], [agent], [model replacement].
fn fixture_frames() -> (Vec<u8>, Vec<Vec<u8>>) {
    let bytes = fs::read(fixture()).unwrap();
    let mut frames = Vec::new();
    let mut offset = HEADER_LEN;
    while offset < bytes.len() {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        frames.push(bytes[offset..offset + 4 + len].to_vec());
        offset += 4 + len;
    }
    assert_eq!(frames.len(), 3);
    (bytes[..HEADER_LEN].to_vec(), frames)
}

/// Replace the only occurrence of a fixed-size ID inside an encoded frame.
fn patch(frame: &mut [u8], from: &[u8], to: &[u8]) {
    let positions: Vec<_> = frame
        .windows(from.len())
        .enumerate()
        .filter(|(_, window)| *window == from)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(positions.len(), 1);
    frame[positions[0]..positions[0] + to.len()].copy_from_slice(to);
}

fn write(temp: &TempDir, name: &str, header: &[u8], frames: &[&[u8]]) -> PathBuf {
    let path = temp.0.join(name);
    fs::write(&path, [header, &frames.concat()].concat()).unwrap();
    path
}

fn diff(a: &Path, b: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_breakspan"))
        .arg("diff")
        .args([a, b])
        .output()
        .unwrap()
}

#[test]
fn identical_recordings_do_not_diverge() {
    let output = diff(&fixture(), &fixture());
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "No divergence: 3 matching steps.\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn diverging_recordings_exit_one_and_rejected_spans_are_reported() {
    let temp = TempDir::new();
    let (header, frames) = fixture_frames();
    // Without the replacement frame, the model keeps its superseded name.
    let superseded = write(
        &temp,
        "superseded.bspan",
        &header,
        &[&frames[0], &frames[1]],
    );
    let expected = "First divergence at step 2\n  \
                      path: agent synthetic agent\n  \
                      a: model synthetic model replacement  depth 1  [ok]\n  \
                      b: model superseded model  depth 1  [ok]\n\
                    1 matching step before divergence.\n";
    let output = diff(&fixture(), &superseded);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    assert!(output.stderr.is_empty());

    // An invalid replacement is rejected; the comparison continues without it.
    let mut invalid = frames[2].clone();
    patch(&mut invalid, &[2; 8], &[0; 8]);
    let rejected = write(
        &temp,
        &format!("{PRIVATE}.bspan"),
        &header,
        &[&frames[0], &frames[1], &invalid],
    );
    let output = diff(&fixture(), &rejected);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Warning: b: 1 spans rejected while loading (invalid spans or memory limits).\n"
    );

    // A strict prefix diverges at its first missing step.
    let prefix = write(&temp, "prefix.bspan", &header, &[&frames[0]]);
    let output = diff(&prefix, &fixture());
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("First divergence at step 1\n  path: (root)\n  a: model superseded model  depth 0  [ok]\n  b: agent synthetic agent  depth 0  [ok]\n")
    );
}

#[test]
fn unusable_recordings_exit_two_without_comparing() {
    let temp = TempDir::new();
    let (header, frames) = fixture_frames();
    let empty = write(&temp, "empty.bspan", &header, &[]);
    let mut other_trace = frames[1].clone();
    patch(&mut other_trace, &[1; 16], &[2; 16]);
    let two_traces = write(&temp, "two.bspan", &header, &[&frames[0], &other_trace]);
    let damaged = temp.0.join(format!("{PRIVATE}-damaged.bspan"));
    fs::write(&damaged, PRIVATE).unwrap();
    let mut truncated = fs::read(fixture()).unwrap();
    truncated.push(0xff); // The next frame's length prefix is truncated.
    let partial = temp.0.join(format!("{PRIVATE}-partial.bspan"));
    fs::write(&partial, truncated).unwrap();

    for (a, b, error) in [
        (&empty, &fixture(), "a: expected exactly one trace, found 0"),
        (
            &fixture(),
            &two_traces,
            "b: expected exactly one trace, found 2",
        ),
        (&damaged, &fixture(), "a: recording: bad magic"),
        (
            &fixture(),
            &partial,
            "b: recording loaded only partially: recording frame 4: truncated frame length",
        ),
        (
            &fixture(),
            &temp.0.join("missing.bspan"),
            "b: recording: could not open file",
        ),
    ] {
        let output = diff(a, b);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr, format!("Error: {error}\n"));
        assert!(!stderr.contains(PRIVATE));
    }
}

#[test]
fn usage_errors_exit_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_breakspan"))
        .args(["diff", "only-one.bspan"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
