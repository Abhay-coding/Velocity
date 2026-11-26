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
const EXCLUSIONS: [&str; 2] = ["**/System Volume Information/**", "**/$RECYCLE.BIN/**"];

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
    if index == 1 {
        "report-needle.txt".into()
    } else if index.is_multiple_of(997) {
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
    fill_corpus(&corpus.0, count)?;
    Ok(corpus)
}

fn fill_corpus(root: &Path, count: usize) -> io::Result<()> {
    let directories = count.div_ceil(FILES_PER_DIRECTORY + 1);
    for directory in 0..directories {
        let path = root.join(format!("group-{directory:04}"));
        fs::create_dir(&path)?;
        let first = directory * FILES_PER_DIRECTORY;
        let last = (first + FILES_PER_DIRECTORY).min(count - directories);
        for index in first..last {
            fs::File::create(path.join(file_name(index)))?;
        }
    }
    Ok(())
}

fn query(text: &str, max_results: usize) -> SearchQuery {
    SearchQuery {
        text: text.to_owned(),
        max_results,
        offset: 0,
    }
}

fn report(label: &str, durations: &mut [u128]) -> u128 {
    durations.sort_unstable();
    let p50 = durations[(durations.len() * 50).div_ceil(100) - 1];
    let p95 = durations[(durations.len() * 95).div_ceil(100) - 1];
    println!(
        "{label},samples={},p50_us={p50},p95_us={p95},values_us={durations:?}",
        durations.len()
    );
    p95
}

#[cfg(windows)]
fn thread_cpu_ticks() -> u64 {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentThread, GetThreadTimes},
    };
    let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
    let ticks =
        |value: FILETIME| (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    assert_ne!(ok, 0, "could not sample benchmark thread CPU time");
    ticks(kernel) + ticks(user)
}

#[cfg(windows)]
async fn profile_query(provider: &FolderProvider, label: &str, text: &str) {
    let mut wall = Vec::with_capacity(SAMPLES);
    let mut cpu = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let before = thread_cpu_ticks();
        let started = Instant::now();
        let page = provider.search(query(text, PAGE_SIZE)).await.unwrap();
        wall.push(started.elapsed().as_micros());
        cpu.push(u128::from(thread_cpu_ticks().saturating_sub(before) / 10));
        std::hint::black_box(page);
    }
    report(&format!("{label}-profile-wall"), &mut wall);
    report(&format!("{label}-profile-thread-cpu"), &mut cpu);
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

async fn wait_for_path(provider: &FolderProvider, text: &str, expected: &Path) {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let page = provider.search(query(text, 1)).await.unwrap();
            if page.total_count == Some(1) && page.results[0].path == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("the changed path should become searchable");
}

async fn wait_for_count(provider: &FolderProvider, count: usize) {
    tokio::time::timeout(Duration::from_secs(120), async {
        while provider.status().indexed_entries != count || !provider.status().searchable {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the expected index should become searchable");
}

fn provider(root: &Path, cache: Option<&Path>) -> Arc<FolderProvider> {
    let exclusions = EXCLUSIONS.map(str::to_owned);
    let provider = FolderProvider::new(vec![root.to_path_buf()], &exclusions).unwrap();
    #[cfg(windows)]
    let provider = match cache {
        Some(cache) => provider
            .with_ntfs_snapshots(velocity_native::ntfs::DEFAULT_BATCH_BYTES)
            .with_snapshot_cache(cache.to_path_buf()),
        None => provider,
    };
    #[cfg(not(windows))]
    assert!(cache.is_none(), "NTFS benchmark requires Windows");
    Arc::new(provider)
}

fn assert_source(provider: &FolderProvider, cache: Option<&Path>, restored: bool) {
    let status = provider.status();
    #[cfg(windows)]
    if cache.is_some() {
        let root = &status.roots[0];
        assert!(
            root.ntfs.is_some(),
            "NTFS benchmark fell back: {:?}",
            root.detail
        );
        assert_eq!(root.ntfs_restored, restored);
        return;
    }
    let _ = (cache, restored);
    assert!(status.searchable);
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
            let index: usize = if name == "report-needle.txt" {
                1
            } else {
                let (stem, _) = name.rsplit_once('.').unwrap();
                stem.rsplit_once('-').unwrap().1.parse().unwrap()
            };
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
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let mut args = std::env::args_os().skip(1);
    let parent = PathBuf::from(
        args.next()
            .expect("supply a local directory for the disposable corpus"),
    );
    let count: usize = args
        .next()
        .expect("supply the entry count")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(count >= 1000);
    let mode = args.next();
    let cache = if mode.as_deref() == Some(std::ffi::OsStr::new("--ntfs")) {
        Some(PathBuf::from(
            args.next().expect("supply a new cache directory"),
        ))
    } else {
        None
    };
    assert!(args.next().is_none());
    let generation = Instant::now();
    let corpus = match mode.as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("--prepare") => {
            fill_corpus(&parent, count)?;
            println!(
                "corpus,entries={count},generation_ms={}",
                generation.elapsed().as_millis()
            );
            return Ok(());
        }
        Some("--folder" | "--ntfs") => None,
        None => {
            let corpus = build_corpus(&fs::canonicalize(&parent)?, count)?;
            println!(
                "corpus,entries={count},generation_ms={}",
                generation.elapsed().as_millis()
            );
            Some(corpus)
        }
        _ => panic!("supported modes are --prepare, --folder and --ntfs"),
    };
    let root = corpus
        .as_ref()
        .map_or(parent.as_path(), |corpus| corpus.0.as_path());
    let cache = cache.as_deref();
    assert!(
        cache.is_none_or(|path| !path.exists()),
        "use a new index cache for each trial"
    );
    println!(
        "provider={},entries={count},page_size={PAGE_SIZE},metadata=false",
        if cache.is_some() { "ntfs" } else { "folder" }
    );

    let provider = provider(root, cache);
    let started = Instant::now();
    let monitor = provider.start_monitor();
    wait_for_count(&provider, count).await;
    assert_source(&provider, cache, false);
    println!("initial_index,ms={}", started.elapsed().as_millis());
    #[cfg(windows)]
    report_memory("memory_after_initial_index");
    if let Some(cache) = cache {
        tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                if fs::read_dir(cache)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .any(|entry| {
                        entry
                            .path()
                            .extension()
                            .is_some_and(|extension| extension == "snapshot")
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the freshly built snapshot should commit before warm queries");
        println!(
            "initial_snapshot_committed,ms={}",
            started.elapsed().as_millis()
        );
    }
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
        let p95 = report(label, &mut durations);
        #[cfg(windows)]
        if p95 > 50_000 || (cache.is_some() && label == "rare" && count == 1_000_000) {
            profile_query(&provider, label, text).await;
        }
        #[cfg(not(windows))]
        let _ = p95;
    }

    let created = root.join("benchmark-change-a.txt");
    let renamed = root.join("benchmark-change-b.txt");
    let moved = root.join("group-0000/benchmark-change-b.txt");
    let mut timings = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for _ in 0..20 {
        let started = Instant::now();
        fs::write(&created, b"benchmark")?;
        wait_for(&provider, "benchmark-change-a", 1).await;
        timings[0].push(started.elapsed().as_micros());
        let started = Instant::now();
        fs::rename(&created, &renamed)?;
        wait_for_path(&provider, "benchmark-change-b", &renamed).await;
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
        fs::rename(&renamed, &moved)?;
        wait_for_path(&provider, "benchmark-change-b", &moved).await;
        timings[2].push(started.elapsed().as_micros());
        let started = Instant::now();
        fs::remove_file(&moved)?;
        wait_for(&provider, "benchmark-change-b", 0).await;
        timings[3].push(started.elapsed().as_micros());
    }
    for (label, timings) in ["create", "rename", "move", "delete"]
        .into_iter()
        .zip(&mut timings)
    {
        report(label, timings);
    }
    compare_corpus(&provider, root, count).await;
    provider.shutdown().await.unwrap();
    monitor.await.unwrap();
    drop(provider);
    let restarted = self::provider(root, cache);
    let started = Instant::now();
    restarted.initialize().await.unwrap();
    println!("warm_restart_no_edits,ms={}", started.elapsed().as_millis());
    assert_eq!(
        restarted.search(query("", 0)).await.unwrap().total_count,
        Some(count)
    );
    assert_source(&restarted, cache, cache.is_some());
    #[cfg(windows)]
    report_memory("memory_after_warm_restart");
    drop(restarted);
    if let Some(cache) = cache {
        let bytes = fs::read_dir(cache)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "snapshot")
            })
            .try_fold(0u64, |total, entry| {
                entry.metadata().map(|metadata| total + metadata.len())
            })?;
        assert!(bytes > 0);
        println!(
            "snapshot,bytes={bytes},bytes_per_entry={:.2}",
            bytes as f64 / count as f64
        );
    }
    fs::write(&created, b"offline edit")?;
    let restarted = self::provider(root, cache);
    let started = Instant::now();
    let initialization = tokio::spawn({
        let restarted = restarted.clone();
        async move { restarted.initialize().await }
    });
    wait_for_count(&restarted, count + 1).await;
    println!(
        "warm_restart_with_offline_edit,ms={}",
        started.elapsed().as_millis()
    );
    initialization.await.unwrap().unwrap();
    assert_source(&restarted, cache, cache.is_some());
    wait_for_path(&restarted, "benchmark-change-a", &created).await;
    fs::remove_file(created)?;
    Ok(())
}
