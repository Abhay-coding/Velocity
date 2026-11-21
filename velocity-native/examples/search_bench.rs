use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use velocity_core::{IndexProvider, SearchQuery};
use velocity_native::FolderProvider;

const SAMPLES: usize = 100;
const PAGE_SIZE: usize = 100;
const FILES_PER_DIRECTORY: usize = 999;

struct Corpus(PathBuf);

impl Drop for Corpus {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "Could not clean benchmark corpus {}: {error}",
                self.0.display()
            );
        }
    }
}

fn file_name(index: usize) -> String {
    if index.is_multiple_of(997) {
        format!("rare-tag-{index:07}.pdf")
    } else {
        format!("budget-report-{index:07}.txt")
    }
}

fn build_corpus(parent: &Path, count: usize) -> io::Result<Corpus> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = parent.join(format!("velocity-bench-{}-{stamp}", std::process::id()));
    fs::create_dir(&root)?;
    let corpus = Corpus(root);
    let directories = count.div_ceil(FILES_PER_DIRECTORY + 1);
    for directory in 0..directories {
        let path = corpus.0.join(format!("group-{directory:04}"));
        fs::create_dir(&path)?;
        let first = directory * FILES_PER_DIRECTORY;
        let last = (first + FILES_PER_DIRECTORY).min(count - directories);
        for index in first..last {
            fs::File::create(path.join(file_name(index)))?;
        }
    }
    Ok(corpus)
}

fn query(text: &str, max_results: usize) -> SearchQuery {
    SearchQuery {
        text: text.to_owned(),
        max_results,
        offset: 0,
    }
}

fn report(label: &str, durations: &mut [u128]) {
    durations.sort_unstable();
    let p50 = durations[(durations.len() * 50).div_ceil(100) - 1];
    let p95 = durations[(durations.len() * 95).div_ceil(100) - 1];
    println!(
        "{label},samples={},p50_us={p50},p95_us={p95},values_us={durations:?}",
        durations.len()
    );
}

#[cfg(windows)]
fn report_memory(label: &str) {
    use windows_sys::Win32::System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
        Threading::GetCurrentProcess,
    };
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    counters.cb = size_of_val(&counters) as u32;
    // The extended structure shares the layout of the base counters prefix.
    if unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            counters.cb,
        )
    } != 0
    {
        println!(
            "{label},working_set_bytes={},peak_working_set_bytes={},private_bytes={}",
            counters.WorkingSetSize, counters.PeakWorkingSetSize, counters.PrivateUsage
        );
    }
}

async fn wait_for(provider: &FolderProvider, text: &str, expected: usize) {
    tokio::time::timeout(Duration::from_secs(90), async {
        while provider
            .search(query(text, 0))
            .await
            .ok()
            .and_then(|page| page.total_count)
            != Some(expected)
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("change should become searchable");
}

async fn compare_corpus(provider: &FolderProvider, root: &Path, count: usize) {
    let all = provider.search(query("", count + 1)).await.unwrap();
    assert_eq!(all.total_count, Some(count));
    let directories = count.div_ceil(FILES_PER_DIRECTORY + 1);
    let mut seen_directories = vec![false; directories];
    let mut seen_files = vec![false; count - directories];
    for row in all.results {
        let relative = row.path.strip_prefix(root).unwrap();
        let mut parts = relative.iter();
        let name = parts.next().unwrap().to_str().unwrap();
        let directory: usize = name.strip_prefix("group-").unwrap().parse().unwrap();
        assert_eq!(name, format!("group-{directory:04}"));
        if row.is_directory {
            assert!(parts.next().is_none());
            assert!(!std::mem::replace(&mut seen_directories[directory], true));
        } else {
            let name = parts.next().unwrap().to_str().unwrap();
            assert!(parts.next().is_none());
            let (stem, _) = name.rsplit_once('.').unwrap();
            let index: usize = stem.rsplit_once('-').unwrap().1.parse().unwrap();
            assert_eq!(directory, index / FILES_PER_DIRECTORY);
            assert_eq!(name, file_name(index));
            assert!(!std::mem::replace(&mut seen_files[index], true));
        }
    }
    assert!(seen_directories.iter().all(|&seen| seen));
    assert!(seen_files.iter().all(|&seen| seen));
    println!("comparison,entries={count},missing=0,extra=0,duplicate=0");
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let parent = PathBuf::from(
        args.next()
            .expect("supply a local directory for the disposable corpus"),
    );
    let parent = fs::canonicalize(parent)?;
    let count: usize = args
        .next()
        .expect("supply the entry count")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(count >= 1000);
    let generation = Instant::now();
    let corpus = build_corpus(&parent, count)?;
    println!(
        "corpus,entries={count},generation_ms={}",
        generation.elapsed().as_millis()
    );

    let provider = Arc::new(FolderProvider::new(vec![corpus.0.clone()], &[]).unwrap());
    let started = Instant::now();
    let monitor = provider.start_monitor();
    wait_for(&provider, "", count).await;
    println!("initial_folder_index,ms={}", started.elapsed().as_millis());
    #[cfg(windows)]
    report_memory("memory_after_initial_index");
    for (label, text) in [
        ("common", "budget-report"),
        ("rare", "rare-tag"),
        ("no-match", "absent-ef8047"),
        ("phrase", "\"budget-report-000\""),
        ("path", "group-0001\\budget"),
        ("extension", "ext:pdf rare"),
    ] {
        for _ in 0..5 {
            provider.search(query(text, PAGE_SIZE)).await.unwrap();
        }
        let mut durations = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            let page = provider.search(query(text, PAGE_SIZE)).await.unwrap();
            durations.push(started.elapsed().as_micros());
            std::hint::black_box(page);
        }
        report(label, &mut durations);
    }

    let created = corpus.0.join("benchmark-change-a.txt");
    let moved = corpus.0.join("group-0000/benchmark-change-b.txt");
    let mut timings = [Vec::new(), Vec::new(), Vec::new()];
    for _ in 0..3 {
        let started = Instant::now();
        fs::write(&created, b"benchmark")?;
        wait_for(&provider, "benchmark-change-a", 1).await;
        timings[0].push(started.elapsed().as_micros());
        let started = Instant::now();
        fs::rename(&created, &moved)?;
        wait_for(&provider, "benchmark-change-b", 1).await;
        assert_eq!(
            provider
                .search(query("benchmark-change-a", 0))
                .await
                .unwrap()
                .total_count,
            Some(0)
        );
        timings[1].push(started.elapsed().as_micros());
        let started = Instant::now();
        fs::remove_file(&moved)?;
        wait_for(&provider, "benchmark-change-b", 0).await;
        timings[2].push(started.elapsed().as_micros());
    }
    for (label, timings) in ["create", "move-and-rename", "delete"]
        .into_iter()
        .zip(&mut timings)
    {
        report(label, timings);
    }
    compare_corpus(&provider, &corpus.0, count).await;
    provider.shutdown().await.unwrap();
    monitor.await.unwrap();
    drop(provider);
    let restarted = FolderProvider::new(vec![corpus.0.clone()], &[]).unwrap();
    let started = Instant::now();
    restarted.initialize().await.unwrap();
    println!(
        "warm_restart_folder_rebuild,ms={}",
        started.elapsed().as_millis()
    );
    assert_eq!(
        restarted.search(query("", 0)).await.unwrap().total_count,
        Some(count)
    );
    Ok(())
}
