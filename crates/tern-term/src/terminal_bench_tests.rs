#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

//! Parse throughput of the model with no window: run
//! `cargo +1.99.1 test -p tern-term --release -- --ignored --nocapture parse_throughput`.

use super::*;
use std::time::Instant;

const TARGET_BYTES: usize = 20 * 1024 * 1024;
/// What one SSH read hands the terminal at most (`tern-ssh` `MAX_CHUNK`).
const CHUNK: usize = 64 * 1024;

/// A cheap deterministic generator, so every run feeds the same bytes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A `cat`-of-a-log line: plain text, a coloured level, a truecolor path, a trailing CRLF.
fn log_line(rng: &mut Rng, out: &mut Vec<u8>) {
    let level = [
        "\x1b[32mINFO\x1b[0m",
        "\x1b[33mWARN\x1b[0m",
        "\x1b[1;31mERROR\x1b[0m",
    ][(rng.next() % 3) as usize];
    let n = rng.next();
    let line = format!(
        "2026-11-05T12:{:02}:{:02}.{:03}Z {level} \x1b[38;2;{};{};{}m/srv/app/{:x}/handler.rs\x1b[0m request {:08x} took {} µs → ok │ ✓\r\n",
        n % 60,
        (n >> 8) % 60,
        (n >> 16) % 1000,
        (n >> 24) % 256,
        (n >> 32) % 256,
        (n >> 40) % 256,
        n >> 48,
        n as u32,
        n % 90_000,
    );
    out.extend_from_slice(line.as_bytes());
}

/// An `htop`-style frame: home the cursor, then rewrite every row with 256-colour bars,
/// bold labels and box-drawing, clearing to the end of each line.
fn full_screen_frame(rng: &mut Rng, rows: usize, out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1b[H");
    for row in 0..rows {
        let bar = 20 + (rng.next() % 60) as usize;
        let colour = 16 + (rng.next() % 200) as u8;
        let row_bytes = format!(
            "\x1b[{};1H\x1b[1m{:>5}\x1b[0m \x1b[38;5;{colour}m{}\x1b[48;5;{colour}m{}\x1b[0m\x1b[K \x1b[2m│\x1b[0m {:5.1}%",
            row + 1,
            rng.next() % 99_999,
            "█".repeat(bar),
            " ".repeat(8),
            (rng.next() % 1000) as f32 / 10.0,
        );
        out.extend_from_slice(row_bytes.as_bytes());
    }
}

fn mixed_output(bytes: usize, rows: usize) -> Vec<u8> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::with_capacity(bytes + 8192);
    while out.len() < bytes {
        // Most of the bytes stream like a log, the rest repaint like a monitor.
        for _ in 0..24 {
            log_line(&mut rng, &mut out);
        }
        full_screen_frame(&mut rng, rows, &mut out);
    }
    out
}

#[test]
#[ignore = "benchmark: run with --release --ignored --nocapture"]
fn parse_throughput() {
    let (cols, rows) = (200, 50);
    let data = mixed_output(TARGET_BYTES, rows);
    let mut t = Terminal::new(cols, rows as u16);

    let started = Instant::now();
    let mut notices = 0;
    for chunk in data.chunks(CHUNK) {
        notices += t.process(chunk).len();
    }
    let parse = started.elapsed();
    assert!(t.history_size() > 0, "the log lines scrolled");

    // What one frame pays on top: a snapshot of the whole viewport.
    let frames = 2000;
    let started = Instant::now();
    let mut cells = 0;
    for _ in 0..frames {
        cells += t.lines().iter().map(Vec::len).sum::<usize>();
    }
    let snapshot = started.elapsed();
    assert_eq!(cells, frames * usize::from(cols) * rows);

    let mib = data.len() as f64 / (1024.0 * 1024.0);
    eprintln!(
        "parse_throughput: {mib:.1} MiB in {:.0} ms = {:.1} MiB/s ({:.1} ns/byte), {notices} notices; \
         lines() {cols}x{rows}: {:.1} us/frame",
        parse.as_secs_f64() * 1e3,
        mib / parse.as_secs_f64(),
        parse.as_nanos() as f64 / data.len() as f64,
        snapshot.as_secs_f64() * 1e6 / frames as f64,
    );
}
