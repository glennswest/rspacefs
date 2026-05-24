//! `rspacefs-bigbust` — async load driver for rspacefs as a containers-storage
//! `mount_program`. Spawns N concurrent short-lived pods across M images and
//! records per-pod timing.
//!
//! Why a Rust binary instead of a bash + kubectl loop:
//!
//! - **No per-call fork overhead.** Bash's `kubectl wait` / `kubectl run`
//!   loop forks a process per pod and serialises through the apiserver one
//!   round-trip at a time. The previous bash bigbust hit a 15-s floor per
//!   pod purely on the wait-for-Ready pattern (issue #10) — that's wasted
//!   wall-clock for a workload that exits in <1 ms.
//! - **True parallelism without GNU parallel.** 200 concurrent tokio tasks
//!   on one apiserver connection vs. 200 forked kubectl processes — the
//!   former is what the apiserver was designed for.
//! - **Watch-stream completion.** We watch the pod once it's created, see
//!   Succeeded in real time, drop the future. No polling, no timeout
//!   guessing.
//!
//! Output: one row per pod into a CSV of (image, name, start_unix_ms,
//! ttr_ms, ttc_ms, phase) plus an aggregated histogram on stdout.

use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;
use k8s_openapi::api::core::v1::{Container, Namespace, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::{
    api::{Api, DeleteParams, PostParams},
    runtime::{watcher, watcher::Event, WatchStreamExt},
    Client,
};
use tokio::sync::Semaphore;

#[derive(Parser, Debug)]
#[command(
    name = "rspacefs-bigbust",
    version,
    about = "Parallel pod load driver for rspacefs / CRI-O"
)]
struct Cli {
    /// Image list. One image ref per line. Lines starting with `#` and
    /// blank lines are ignored.
    #[arg(long, default_value = "tests/k8s/workloads/bigbust-images.txt")]
    images: std::path::PathBuf,

    /// Namespace pods land in. Created if missing.
    #[arg(long, default_value = "rspacefs-bigbust")]
    namespace: String,

    /// How many pods to run concurrently. The whole grid (runs × images)
    /// is enqueued at once; this is the wide cap on inflight pods.
    #[arg(long, default_value_t = 200)]
    parallel: usize,

    /// How many runs per image. Total pods = runs × images. With the
    /// defaults (4 × 50 = 200 pods) and --parallel=200, the whole load
    /// fires in one wave.
    #[arg(long, default_value_t = 4)]
    runs_per_image: usize,

    /// Cap on total pods (across all images). Truncates the grid; useful
    /// for quick smokes. 0 = no cap.
    #[arg(long, default_value_t = 0)]
    max_pods: usize,

    /// Per-pod timeout (the apiserver tells us pod transitioned to
    /// Succeeded or Failed within this window, else we record Timeout).
    /// 60 s is generous; real workloads (busybox `true`) finish in <1 s.
    #[arg(long, default_value_t = 60)]
    timeout_secs: u64,

    /// Output CSV path. Defaults to /tmp/rspacefs-bigbust-<unix-ms>.csv.
    #[arg(long)]
    out: Option<std::path::PathBuf>,

    /// Skip the image pre-pull phase. The watch-stream sees pulls as part
    /// of pod lifecycle, but a fresh node usually wants pre-warming so the
    /// timing numbers aren't dominated by registry RTT.
    #[arg(long)]
    skip_prepull: bool,
}

#[derive(Debug, Clone)]
struct RunResult {
    image: String,
    name: String,
    start_unix_ms: u128,
    /// Time-to-running: pod CREATED → pod observed in Running phase.
    ttr_ms: Option<u128>,
    /// Time-to-complete: pod CREATED → Succeeded or Failed.
    ttc_ms: Option<u128>,
    phase: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rspacefs_bigbust=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let client = Client::try_default().await.context("kube client")?;

    // Ensure namespace.
    let ns_api: Api<Namespace> = Api::all(client.clone());
    let _ = ns_api
        .create(
            &PostParams::default(),
            &Namespace {
                metadata: ObjectMeta {
                    name: Some(cli.namespace.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await; // ignore AlreadyExists

    // Load image list.
    let raw = std::fs::read_to_string(&cli.images)
        .with_context(|| format!("reading {}", cli.images.display()))?;
    let images: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect();
    tracing::info!(
        images = images.len(),
        parallel = cli.parallel,
        runs_per_image = cli.runs_per_image,
        "loaded image list"
    );

    // Build the grid: every image × runs_per_image.
    let mut grid: Vec<(String, usize)> = Vec::new();
    for (img_idx, img) in images.iter().enumerate() {
        for r in 0..cli.runs_per_image {
            grid.push((img.clone(), img_idx * cli.runs_per_image + r));
        }
    }
    if cli.max_pods > 0 && grid.len() > cli.max_pods {
        grid.truncate(cli.max_pods);
    }
    let total = grid.len();
    tracing::info!(total_pods = total, "grid built; firing");

    // Pre-pull all images via a tiny init pod per image — optional. Skipped
    // by default in the parallel bigbust because the goal IS to measure the
    // cold-pull side of rspacefs's mount_program path too.
    if !cli.skip_prepull {
        prepull(&client, &cli.namespace, &images).await?;
    }

    let pods_api: Api<Pod> = Api::namespaced(client.clone(), &cli.namespace);
    let sem = Arc::new(Semaphore::new(cli.parallel));

    let started_at = Instant::now();
    let mut tasks = Vec::with_capacity(total);
    for (image, n) in grid {
        let permit = Arc::clone(&sem).acquire_owned().await?;
        let pods_api = pods_api.clone();
        let name = format!("bigbust-{}", n);
        let timeout = std::time::Duration::from_secs(cli.timeout_secs);
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            run_one(pods_api, image, name, timeout).await
        }));
    }

    // Collect. The concurrency is already happening via the semaphore
    // (200 tasks queued, ≤parallel inflight). Awaiting in submission
    // order just drains finished tasks; not order-sensitive.
    let mut results: Vec<RunResult> = Vec::with_capacity(total);
    let mut joined = 0;
    for t in tasks {
        match t.await {
            Ok(r) => results.push(r),
            Err(e) => tracing::warn!(?e, "task panicked"),
        }
        joined += 1;
        if joined % 20 == 0 {
            tracing::info!(done = joined, of = total, "progress");
        }
    }
    let elapsed = started_at.elapsed();

    // Write CSV.
    let out_path = cli.out.unwrap_or_else(|| {
        std::path::PathBuf::from(format!(
            "/tmp/rspacefs-bigbust-{}.csv",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ))
    });
    write_csv(&out_path, &results)?;
    tracing::info!(out = %out_path.display(), "wrote csv");

    // Summary on stdout.
    summarise(&results, elapsed);

    Ok(())
}

async fn prepull(client: &Client, ns: &str, images: &[String]) -> Result<()> {
    // Cheapest pre-pull: fire `kubectl run --rm` equivalents in parallel,
    // 4-wide, that pull only and exit. The runtime caches images in the
    // CRI-O store, so subsequent bigbust runs see warm pulls.
    use tokio::sync::Semaphore;
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let sem = Arc::new(Semaphore::new(4));
    let mut tasks = Vec::with_capacity(images.len());
    for (i, image) in images.iter().enumerate() {
        let permit = Arc::clone(&sem).acquire_owned().await?;
        let pods_api = pods_api.clone();
        let image = image.clone();
        let name = format!("bigbust-prepull-{}", i);
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let _ = run_one(pods_api, image, name, std::time::Duration::from_secs(300)).await;
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    tracing::info!("pre-pull phase done");
    Ok(())
}

async fn run_one(
    pods_api: Api<Pod>,
    image: String,
    name: String,
    timeout: std::time::Duration,
) -> RunResult {
    let started_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let t0 = Instant::now();

    let pod = Pod {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            containers: vec![Container {
                name: "x".into(),
                image: Some(image.clone()),
                command: Some(vec!["/bin/sh".into(), "-c".into(), "true".into()]),
                image_pull_policy: Some("IfNotPresent".into()),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };

    if let Err(e) = pods_api.create(&PostParams::default(), &pod).await {
        return RunResult {
            image,
            name,
            start_unix_ms: started_unix_ms,
            ttr_ms: None,
            ttc_ms: None,
            phase: format!("create-error: {}", e),
        };
    }

    // Watch this pod by name. The watcher abstraction relabels Apply/Delete
    // events into a stream of Pod states; we look for phase transitions.
    let conf = watcher::Config::default()
        .fields(&format!("metadata.name={}", name))
        .timeout(timeout.as_secs() as u32);
    let mut ttr_ms: Option<u128> = None;
    let mut ttc_ms: Option<u128> = None;
    let mut final_phase = "Timeout".to_string();

    let mut stream = watcher(pods_api.clone(), conf).default_backoff().boxed();
    let deadline = tokio::time::Instant::now() + timeout;
    while let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        let next = tokio::time::timeout(remaining, stream.next()).await;
        let event = match next {
            Ok(Some(Ok(e))) => e,
            Ok(Some(Err(_))) | Err(_) | Ok(None) => break,
        };
        let pod = match event {
            Event::Apply(p) | Event::InitApply(p) => p,
            _ => continue,
        };
        let phase = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.as_deref())
            .unwrap_or("")
            .to_string();
        if ttr_ms.is_none() && phase == "Running" {
            ttr_ms = Some(t0.elapsed().as_millis());
        }
        if phase == "Succeeded" || phase == "Failed" {
            ttc_ms = Some(t0.elapsed().as_millis());
            final_phase = phase;
            break;
        }
    }

    // Best-effort cleanup. wait=false so we don't block on terminating.
    let _ = pods_api
        .delete(&name, &DeleteParams::default().grace_period(0))
        .await;

    RunResult {
        image,
        name,
        start_unix_ms: started_unix_ms,
        ttr_ms,
        ttc_ms,
        phase: final_phase,
    }
}

fn write_csv(path: &std::path::Path, results: &[RunResult]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "image,name,start_unix_ms,ttr_ms,ttc_ms,phase")?;
    for r in results {
        writeln!(
            f,
            "{},{},{},{},{},{}",
            r.image,
            r.name,
            r.start_unix_ms,
            r.ttr_ms.map(|v| v.to_string()).unwrap_or_default(),
            r.ttc_ms.map(|v| v.to_string()).unwrap_or_default(),
            r.phase,
        )?;
    }
    Ok(())
}

fn summarise(results: &[RunResult], elapsed: std::time::Duration) {
    let mut succ: Vec<u128> = results
        .iter()
        .filter(|r| r.phase == "Succeeded")
        .filter_map(|r| r.ttc_ms)
        .collect();
    succ.sort_unstable();
    let n = succ.len();
    let total = results.len();
    let failures = total - n;

    let median = if n == 0 { 0 } else { succ[n / 2] };
    let p90 = if n == 0 { 0 } else { succ[(n * 9) / 10] };
    let p99 = if n == 0 { 0 } else { succ[(n * 99) / 100] };
    let max = if n == 0 { 0 } else { *succ.last().unwrap() };
    let mean = if n == 0 {
        0
    } else {
        succ.iter().sum::<u128>() / n as u128
    };

    println!();
    println!("=== bigbust summary ===");
    println!("total pods           {}", total);
    println!("succeeded            {}", n);
    println!("failed / timeout     {}", failures);
    println!("wall clock           {:.1?}", elapsed);
    println!(
        "throughput           {:.1} pods/sec",
        total as f64 / elapsed.as_secs_f64()
    );
    println!();
    println!("ttc (succeeded pods, ms):");
    println!("  median             {}", median);
    println!("  p90                {}", p90);
    println!("  p99                {}", p99);
    println!("  max                {}", max);
    println!("  mean               {}", mean);
}
