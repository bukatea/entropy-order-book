//! End-to-end tests of the `orderbook` binary: exact stdout bytes, exit status
//! and the input file's bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const HEADER: &str = "seq,cmd,id,owner,side,price,size,tif";

/// The path of a file under `tests/fixtures`.
fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_str()
        .expect("UTF-8 path")
        .to_owned()
}

/// Runs the binary with `args`, from the test's working directory.
fn orderbook(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orderbook"))
        .args(args)
        .output()
        .expect("run orderbook")
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn lines(stdout: &[u8]) -> Vec<String> {
    String::from_utf8(stdout.to_vec())
        .expect("UTF-8 output")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A scratch directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("orderbook-cli-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    /// Writes `bytes` to a new file `name` and returns its path.
    fn file(&self, name: &str, bytes: impl AsRef<[u8]>) -> String {
        let path = self.0.join(name);
        fs::write(&path, bytes).expect("write temp file");
        path.to_str().expect("UTF-8 path").to_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The sample log's golden event stream, then its digest trailer.
fn sample_golden_events() -> Vec<u8> {
    let mut bytes = fs::read(fixture("sample.events")).expect("read sample.events");
    let digest = fs::read_to_string(fixture("sample.blake3")).expect("read sample.blake3");
    bytes.extend_from_slice(format!("digest,blake3,{}\n", digest.trim()).as_bytes());
    bytes
}

fn sample_golden_state() -> Vec<u8> {
    fs::read(fixture("sample.state")).expect("read sample.state")
}

/// `events` followed by the digest trailer. The digest is BLAKE3 over the
/// canonical state bytes, so `state` is the output of `--state` for the same log.
fn with_trailer(events: &str, state: &[u8]) -> Vec<u8> {
    let digest = blake3::hash(state);
    format!("{events}digest,blake3,{}\n", digest.to_hex()).into_bytes()
}

// exkg-verifies: fea-ky6m7m#ac-cli
// exkg-verifies: fea-ky6m7m#ac-determinism
#[test]
fn sample_events_match_goldens_in_separate_processes() {
    let sample = fixture("sample.csv");
    let expected = sample_golden_events();
    for _ in 0..2 {
        let output = orderbook(&[sample.as_str()]);
        assert_exit(&output, 0);
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout, expected);
    }
}

// exkg-verifies: fea-ky6m7m#ac-cli
// exkg-verifies: fea-ky6m7m#ac-determinism
#[test]
fn sample_state_matches_golden_in_separate_processes() {
    let sample = fixture("sample.csv");
    let expected = sample_golden_state();
    for _ in 0..2 {
        let output = orderbook(&["--state", sample.as_str()]);
        assert_exit(&output, 0);
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout, expected);
    }
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn malformed_header_halts_with_header_mismatch() {
    let dir = TempDir::new("header-mismatch");
    let input = dir.file(
        "bad.csv",
        "seq,cmd,id,owner,side,price,size,tif,extra\n1,cancel,a1,,,,,\n",
    );
    let output = orderbook(&[input.as_str()]);
    let state = orderbook(&["--state", input.as_str()]);
    assert_exit(&output, 1);
    assert_exit(&state, 1);
    assert_eq!(
        output.stdout,
        with_trailer(
            "eob-events,1\n1,halted,header_mismatch,none\n",
            &state.stdout
        )
    );
    assert!(String::from_utf8_lossy(&state.stdout).contains("halted,header_mismatch\n"));
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn empty_file_halts_with_header_empty() {
    let dir = TempDir::new("header-empty");
    let input = dir.file("empty.csv", "");
    let output = orderbook(&[input.as_str()]);
    let state = orderbook(&["--state", input.as_str()]);
    assert_exit(&output, 1);
    assert_exit(&state, 1);
    assert_eq!(
        output.stdout,
        with_trailer("eob-events,1\n1,halted,header_empty,none\n", &state.stdout)
    );
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn sequence_gap_halts_and_skips_later_lines() {
    let dir = TempDir::new("seq-gap");
    let input = dir.file(
        "gap.csv",
        format!(
            "{HEADER}\n\
             1,place,a1,alice,buy,100.25,5,GTC\n\
             3,place,b1,bob,sell,100.50,3,GTC\n\
             2,place,c1,carol,sell,100.50,1,GTC\n"
        ),
    );
    let output = orderbook(&[input.as_str()]);
    assert_exit(&output, 1);
    let text = String::from_utf8(output.stdout.clone()).expect("UTF-8 output");
    assert!(text.contains("1,accepted,a1,alice,"));
    assert!(text.contains("halted,seq_gap"));
    assert!(!text.contains("bob"));
    assert!(!text.contains("carol"));

    let state = orderbook(&["--state", input.as_str()]);
    assert_exit(&state, 1);
    let state_text = String::from_utf8(state.stdout).expect("UTF-8 output");
    assert!(state_text.contains("next_seq,2\n"));
    assert!(state_text.contains("halted,seq_gap\n"));
    assert!(state_text.contains("order,a1,alice,"));
    assert!(!state_text.contains("bob"));
    assert!(!state_text.contains("carol"));
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn malformed_sequence_halts_with_observed_none() {
    let dir = TempDir::new("seq-syntax");
    let input = dir.file(
        "seq.csv",
        format!(
            "{HEADER}\n\
             1,place,a1,alice,buy,100.25,5,GTC\n\
             x,place,b1,bob,sell,100.50,3,GTC\n"
        ),
    );
    let output = orderbook(&[input.as_str()]);
    let state = orderbook(&["--state", input.as_str()]);
    assert_exit(&output, 1);
    assert_exit(&state, 1);
    assert_eq!(
        output.stdout,
        with_trailer(
            "eob-events,1\n\
             1,accepted,a1,alice,buy,100.25,5,GTC\n\
             1,rested,a1,buy,100.25,5,1\n\
             2,halted,seq_syntax,none\n",
            &state.stdout
        )
    );
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn rejected_rows_exit_zero_and_are_reported() {
    let dir = TempDir::new("rejects");
    let input = dir.file(
        "rejects.csv",
        format!(
            "{HEADER}\n\
             1,place,a1,alice,buy,0,5,GTC\n\
             2,frobnicate,a1,,,,,\n\
             3,place,a2,alice,sell,100.25,5,GTC\n"
        ),
    );
    let output = orderbook(&[input.as_str()]);
    assert_exit(&output, 0);
    assert!(output.stderr.is_empty());
    let text = lines(&output.stdout);
    assert_eq!(text.first().map(String::as_str), Some("eob-events,1"));
    assert!(text.iter().any(|l| l.starts_with("1,rejected,input,,")));
    assert!(text.iter().any(|l| l.starts_with("2,rejected,input,,")));
    assert!(text.iter().any(|l| l.starts_with("3,accepted,a2,")));
    assert!(text.last().is_some_and(|l| l.starts_with("digest,blake3,")));
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn crlf_and_missing_final_newline_match_lf_output() {
    let dir = TempDir::new("line-endings");
    let lf = fs::read_to_string(fixture("sample.csv")).expect("read sample.csv");
    let crlf = lf.replace('\n', "\r\n");
    let inputs = [
        dir.file("crlf.csv", &crlf),
        dir.file(
            "bare.csv",
            lf.strip_suffix('\n').expect("sample ends with LF"),
        ),
        dir.file(
            "crlf-bare.csv",
            crlf.strip_suffix("\r\n").expect("sample ends with CRLF"),
        ),
    ];
    for input in &inputs {
        let output = orderbook(&[input.as_str()]);
        assert_exit(&output, 0);
        assert_eq!(output.stdout, sample_golden_events(), "input {input}");
        let state = orderbook(&["--state", input.as_str()]);
        assert_exit(&state, 0);
        assert_eq!(state.stdout, sample_golden_state(), "input {input}");
    }
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn usage_errors_exit_two_with_empty_stdout() {
    let sample = fixture("sample.csv");
    let sample = sample.as_str();
    let cases: [&[&str]; 4] = [&["--bogus", sample], &[], &[sample, sample], &["-x"]];
    for args in cases {
        let output = orderbook(args);
        assert_exit(&output, 2);
        assert!(output.stdout.is_empty(), "args {args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage:"),
            "args {args:?}"
        );
    }
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn help_prints_usage_and_exits_zero() {
    for flag in ["--help", "-h"] {
        let output = orderbook(&[flag]);
        assert_exit(&output, 0);
        assert!(String::from_utf8_lossy(&output.stdout).contains("usage: orderbook"));
        assert!(output.stderr.is_empty());
    }
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn missing_file_exits_three_with_empty_stdout() {
    let dir = TempDir::new("missing");
    let missing = dir.0.join("missing.csv");
    let output = orderbook(&[missing.to_str().expect("UTF-8 path")]);
    assert_exit(&output, 3);
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn double_dash_accepts_dash_prefixed_file_name() {
    let dir = TempDir::new("dash-name");
    fs::copy(fixture("sample.csv"), dir.0.join("-log.csv")).expect("copy sample");
    let output = Command::new(env!("CARGO_BIN_EXE_orderbook"))
        .current_dir(&dir.0)
        .args(["--", "-log.csv"])
        .output()
        .expect("run orderbook");
    assert_exit(&output, 0);
    assert_eq!(output.stdout, sample_golden_events());
}

// exkg-verifies: fea-ky6m7m#ac-cli
#[test]
fn input_file_bytes_are_unchanged() {
    let dir = TempDir::new("unchanged");
    let original = fs::read(fixture("sample.csv")).expect("read sample.csv");
    let input = dir.file("log.csv", &original);
    assert_exit(&orderbook(&[input.as_str()]), 0);
    assert_exit(&orderbook(&["--state", input.as_str()]), 0);
    assert_eq!(fs::read(&input).expect("re-read input"), original);
}
