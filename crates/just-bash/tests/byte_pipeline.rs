//! Byte-fidelity regression tests for the output pipeline.
//!
//! The runtime carries command output as raw bytes (`Vec<u8>`) end to end:
//! standard output, pipes, redirection into the virtual filesystem, and the
//! public [`just_bash::ExecResult`]. These tests pin that behaviour down for the
//! byte-oriented builtins, because a `String`-based pipeline silently rewrote
//! every byte >= 0x80 as UTF-8 (e.g. `printf '\xff'` produced `c3 bf`).

use just_bash::{Bash, Options};

fn bash() -> Bash {
    Bash::new(Options::default()).unwrap()
}

/// `printf '\xff'` must be exactly one byte, and `wc -c` must count bytes.
#[test]
fn printf_high_byte_is_a_single_byte() {
    let result = bash().exec(r"printf '\xff' | wc -c");
    assert_eq!(result.stdout, b"1\n");
}

/// The same byte survives a trip through the virtual filesystem.
#[test]
fn high_byte_roundtrips_through_a_file() {
    let mut shell = bash();
    let result = shell.exec(r"printf '\xff' > /f; wc -c < /f");
    assert_eq!(result.stdout, b"1\n");

    let dumped = shell.exec("xxd -p < /f");
    assert_eq!(dumped.stdout, b"ff\n");
}

/// Hidden NUL bytes and high bytes are preserved through a pipe into `xxd`.
#[test]
fn pipe_preserves_nul_and_high_bytes() {
    let result = bash().exec(r"printf '\x00\xffAB' | xxd -p");
    assert_eq!(result.stdout, b"00ff4142\n");
}

/// `xxd -r` reconstructs the exact original bytes (no replacement characters).
#[test]
fn xxd_reverse_is_byte_exact() {
    let result = bash().exec("printf 'ff' | xxd -r -p | xxd -p");
    assert_eq!(result.stdout, b"ff\n");

    let ascii = bash().exec("printf '414243' | xxd -r -p");
    assert_eq!(ascii.stdout, b"ABC");
}

/// `cat` and `tee` are byte-transparent.
#[test]
fn cat_and_tee_are_byte_transparent() {
    let result = bash().exec(r"printf 'a\xffb' | cat | xxd -p");
    assert_eq!(result.stdout, b"61ff62\n");

    let mut shell = bash();
    let tee = shell.exec(r"printf 'a\xffb' | tee /g | xxd -p");
    assert_eq!(tee.stdout, b"61ff62\n");
    assert_eq!(shell.exec("xxd -p < /g").stdout, b"61ff62\n");
}

/// `head -c` / `tail -c` select raw bytes, not characters.
#[test]
fn head_tail_byte_counts_are_exact() {
    assert_eq!(
        bash()
            .exec("printf '0123456789' | head -c 3 | xxd -p")
            .stdout,
        b"303132\n"
    );
    assert_eq!(
        bash()
            .exec("printf '0123456789' | tail -c 3 | xxd -p")
            .stdout,
        b"373839\n"
    );
}

/// `base64` round-trips arbitrary bytes.
#[test]
fn base64_roundtrips_binary() {
    let result = bash().exec(r"printf '\x00\xff\x10' | base64");
    assert_eq!(result.stdout, b"AP8Q\n");

    let decoded = bash().exec(r"printf '\x00\xff\x10' | base64 | base64 -d | xxd -p");
    assert_eq!(decoded.stdout, b"00ff10\n");
}

/// Literal (non-escaped) UTF-8 in a `printf` format stays valid UTF-8.
#[test]
fn printf_literal_utf8_is_preserved() {
    let result = bash().exec("printf 'héllo'");
    assert_eq!(result.stdout, "héllo".as_bytes());
}

/// The public `ExecResult` exposes both raw bytes and a lossy text view.
#[test]
fn exec_result_exposes_bytes_and_text() {
    let result = bash().exec("printf 'hi'");
    assert_eq!(result.stdout, b"hi");
    assert_eq!(result.stdout_text(), "hi");
    assert_eq!(result.stderr, b"");
    assert_eq!(result.stderr_text(), "");
}
