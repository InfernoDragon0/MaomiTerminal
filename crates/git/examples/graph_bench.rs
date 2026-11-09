//! Commit graph bench on a real repository: the same feed the graph view runs
//! (`log_stream(LogQuery::graph())` -> `History::extend` per batch), timed without and with
//! the commit-graph file, plus viewport queries and memory.
//!
//! `cargo run -p maomi-git --release --example graph_bench -- <repo> [--runs 3] [--skip-cold]`
//!
//! The cold phase turns `core.commitGraph` off in the repo's own config for its run and
//! removes the setting afterwards, so only point it at a bench clone.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use maomi_git::graph::History;
use maomi_git::{CliRepo, GitRunner, LogQuery};

struct Load {
    first_rows: Duration,
    full: Duration,
    commits: usize,
    history: History,
}

async fn load(repo: &CliRepo) -> Load {
    let started = Instant::now();
    let mut stream = repo.log_stream(&LogQuery::graph()).expect("git log");
    let mut history = History::new();
    let mut first_rows = None;
    while let Some(batch) = stream.next_batch().await.expect("log batch") {
        history.extend(batch);
        first_rows.get_or_insert_with(|| started.elapsed());
    }
    Load {
        first_rows: first_rows.unwrap_or_default(),
        full: started.elapsed(),
        commits: history.len(),
        history,
    }
}

fn git_config(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("config")
        .args(args)
        .status()
        .expect("git config");
    // `--unset` of a missing key exits 5: fine.
    assert!(
        status.success() || status.code() == Some(5),
        "git config {args:?}"
    );
}

fn report(label: &str, load: &Load) {
    println!(
        "{label:<22} first rows {:>9.1?}  full load {:>9.1?}  {} commits  ({:.2} us/commit)  ws {}",
        load.first_rows,
        load.full,
        load.commits,
        load.full.as_secs_f64() * 1e6 / load.commits.max(1) as f64,
        memory(),
    );
}

/// Viewport cost: spans + row data for a 60-row window at ~1000 positions.
fn viewport(history: &History) {
    let n = history.len();
    if n == 0 {
        return;
    }
    let step = (n / 1000).max(1);
    let started = Instant::now();
    let (mut queries, mut spans, mut chars) = (0u32, 0usize, 0usize);
    for top in (0..n).step_by(step) {
        let rows = top..(top + 60).min(n);
        spans += history.layout().spans(rows.clone()).count();
        for row in rows {
            chars += history.row(row).map_or(0, |r| r.subject.len());
        }
        queries += 1;
    }
    println!(
        "viewport (60 rows)     {:?} avg over {queries} queries ({} spans/window avg), lane width {}  [{chars}]",
        started.elapsed() / queries,
        spans / queries as usize,
        history.layout().width(),
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut repo_path: Option<PathBuf> = None;
    let mut runs = 3;
    let mut skip_cold = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => runs = args.next().and_then(|n| n.parse().ok()).expect("--runs N"),
            "--skip-cold" => skip_cold = true,
            path => repo_path = Some(path.into()),
        }
    }
    let repo_path = repo_path.expect("usage: graph_bench <repo> [--runs N] [--skip-cold]");

    async_io::block_on(async {
        let runner = GitRunner::detect().expect("git");
        let repo = CliRepo::open(runner, &repo_path).await.expect("open repo");
        println!("repo {}  ws at start {}", repo_path.display(), memory());

        // The graph view's first-load preview: date order, one screen.
        let started = Instant::now();
        let preview = LogQuery {
            topo_order: false,
            limit: Some(200),
            ..LogQuery::graph()
        };
        let mut stream = repo.log_stream(&preview).expect("git log");
        let mut rows = 0;
        while let Some(batch) = stream.next_batch().await.expect("log batch") {
            rows += batch.len();
        }
        println!(
            "date-order preview     {rows} rows in {:.1?}",
            started.elapsed()
        );

        if !skip_cold {
            git_config(&repo_path, &["core.commitGraph", "false"]);
            let cold = load(&repo).await;
            git_config(&repo_path, &["--unset", "core.commitGraph"]);
            report("no commit-graph", &cold);
        }

        let started = Instant::now();
        let outcome = repo.ensure_commit_graph().await.expect("commit-graph");
        println!(
            "ensure_commit_graph    {:?} in {:.1?}",
            outcome,
            started.elapsed()
        );

        let mut last = None;
        for run in 1..=runs {
            drop(last.take());
            let warm = load(&repo).await;
            report(&format!("commit-graph run {run}"), &warm);
            last = Some(warm);
        }
        if let Some(last) = &last {
            viewport(&last.history);
            println!("peak working set       {}", peak_memory());
        }
    });
}

fn mib(bytes: usize) -> String {
    format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
}

#[cfg(windows)]
fn counters() -> Option<(usize, usize)> {
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[allow(unsafe_code)]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }
    let mut c = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    // SAFETY: a correctly sized, writable struct for the current-process pseudo handle.
    #[allow(unsafe_code)]
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (ok != 0).then_some((c.working_set_size, c.peak_working_set_size))
}

#[cfg(not(windows))]
fn counters() -> Option<(usize, usize)> {
    None
}

fn memory() -> String {
    counters().map_or("n/a".into(), |(ws, _)| mib(ws))
}

fn peak_memory() -> String {
    counters().map_or("n/a".into(), |(_, peak)| mib(peak))
}
