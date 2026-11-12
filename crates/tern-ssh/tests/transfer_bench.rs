//! Transfer queue timing between two in-process SFTP hosts, each behind a link with added
//! one-way delay.
//! Ignored by default: `cargo test -p tern-ssh --release --test transfer_bench -- --ignored --nocapture`
//! (see `docs/perf.md`, "Transfer pipelining"). Env: `TERN_BENCH_MIB` (default 32),
//! `TERN_BENCH_DELAY_MS` one way (default 10), `TERN_BENCH_RUNS` (default 5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]

mod common;

use std::time::{Duration, Instant};

use std::sync::Arc;

use common::{serve_sftp, spec, start};
use tern_ssh::{CopyJob, Event, FileSource, SftpFiles, TransferQueue};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A TCP relay that holds every chunk for `delay` before forwarding it, in both directions.
async fn slow_link(upstream: u16, delay: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let server = TcpStream::connect(("127.0.0.1", upstream)).await.unwrap();
            let (cr, cw) = client.into_split();
            let (sr, sw) = server.into_split();
            tokio::spawn(delayed(cr, sw, delay));
            tokio::spawn(delayed(sr, cw, delay));
        }
    });
    port
}

async fn delayed(
    mut from: impl AsyncReadExt + Unpin + Send + 'static,
    mut to: impl AsyncWriteExt + Unpin + Send + 'static,
    delay: Duration,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(Instant, Vec<u8>)>(4096);
    tokio::spawn(async move {
        while let Some((due, bytes)) = rx.recv().await {
            tokio::time::sleep_until(due.into()).await;
            if to.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let mut buf = vec![0u8; 32 * 1024];
    while let Ok(n) = from.read(&mut buf).await {
        if n == 0
            || tx
                .send((Instant::now() + delay, buf[..n].to_vec()))
                .await
                .is_err()
        {
            break;
        }
    }
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Host {
    source: Arc<dyn FileSource>,
    root: tempfile::TempDir,
    _live: common::Live,
    _keys: tempfile::TempDir,
}

async fn host(id: &str, delay: Duration) -> Host {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("home")).unwrap();
    let keys = tempfile::tempdir().unwrap();
    let server = serve_sftp(root.path().to_path_buf()).await;
    let port = slow_link(server.port, delay).await;
    let live = start(spec(port, &keys)).await.unwrap();
    let sftp = live.handle.open_sftp().await.unwrap();
    Host {
        source: Arc::new(SftpFiles::new(id, sftp)),
        root,
        _live: live,
        _keys: keys,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "timing run, see docs/perf.md"]
async fn host_to_host_copy_time_over_slow_links() {
    let (mib, delay_ms, runs) = (
        env_or("TERN_BENCH_MIB", 32) as usize,
        env_or("TERN_BENCH_DELAY_MS", 10),
        env_or("TERN_BENCH_RUNS", 5),
    );
    let delay = Duration::from_millis(delay_ms);
    let (a, b) = (host("a", delay).await, host("b", delay).await);
    let data: Vec<u8> = (0..mib * 1024 * 1024)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect();
    std::fs::write(a.root.path().join("home/big.bin"), &data).unwrap();
    let item = a
        .source
        .list("/home")
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.name == "big.bin")
        .unwrap();
    let (queue, events) = TransferQueue::new(tokio::runtime::Handle::current());

    let mut secs = Vec::new();
    for _ in 0..runs {
        let _ = std::fs::remove_file(b.root.path().join("home/big.bin"));
        let t = Instant::now();
        queue.submit(vec![CopyJob {
            from: a.source.clone(),
            item: item.clone(),
            to: b.source.clone(),
            dir: "/home".to_owned(),
            rule: None,
        }]);
        loop {
            if let Event::Finished(s) = events.recv().await.unwrap() {
                assert_eq!((s.copied, s.failed.len()), (1, 0), "{:?}", s.failed);
                break;
            }
        }
        secs.push(t.elapsed().as_secs_f64());
    }
    assert_eq!(
        std::fs::read(b.root.path().join("home/big.bin")).unwrap(),
        data
    );
    secs.sort_by(f64::total_cmp);
    let median = secs[secs.len() / 2];
    println!(
        "{mib} MiB host to host, {delay_ms} ms one way on each link, {runs} runs: median {median:.2} s ({:.1} MiB/s) {secs:.2?}",
        mib as f64 / median
    );
}
