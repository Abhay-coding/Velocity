use super::*;
use crate::{FolderProvider, IndexProvider, SearchQuery};
use std::{collections::BTreeSet, os::windows::fs::MetadataExt, process::Command, time::Duration};

fn fixture() -> (PathBuf, PathBuf, PathBuf) {
    let get = |name| PathBuf::from(std::env::var_os(name).unwrap());
    (
        get("VELOCITY_NTFS_CACHE"),
        get("VELOCITY_NTFS_VOLUME_A"),
        get("VELOCITY_NTFS_VOLUME_B"),
    )
}

fn provider(cache: &Path, roots: &[PathBuf], extra_exclusion: Option<&str>) -> Arc<FolderProvider> {
    let exclusions: Vec<_> = [
        "**/System Volume Information/**",
        "**/$RECYCLE.BIN/**",
        "**/excluded/**",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(extra_exclusion.map(|directory| format!("**/{directory}/**")))
    .collect();
    Arc::new(
        FolderProvider::new(roots.to_vec(), &exclusions)
            .unwrap()
            .with_ntfs_snapshots(super::super::super::DEFAULT_BATCH_BYTES)
            .with_snapshot_cache(cache.to_path_buf()),
    )
}

fn coverage(provider: &FolderProvider, root: &Path) -> RootCoverage {
    provider
        .status()
        .roots
        .into_iter()
        .find(|item| item.root == root)
        .unwrap()
}

fn walk(root: &Path, extra_exclusion: Option<&str>) -> BTreeSet<(PathBuf, bool)> {
    let mut result = BTreeSet::new();
    let mut pending = vec![fs::read_dir(root).unwrap()];
    while let Some(children) = pending.pop() {
        for entry in children {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if ["System Volume Information", "$RECYCLE.BIN", "excluded"]
                .into_iter()
                .chain(extra_exclusion)
                .any(|skip| name.eq_ignore_ascii_case(skip))
            {
                continue;
            }
            let path = entry.path();
            let metadata = entry.metadata().unwrap();
            if metadata.file_attributes() & 0x400 != 0 {
                continue;
            }
            if metadata.is_dir() {
                match fs::read_dir(&path) {
                    Ok(children) => pending.push(children),
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => continue,
                    Err(error) => panic!("independent directory walk failed: {error}"),
                }
            }
            result.insert((path, metadata.is_dir()));
        }
    }
    result
}

async fn assert_walk(provider: &FolderProvider, roots: &[PathBuf], extra_exclusion: Option<&str>) {
    let expected: BTreeSet<_> = roots
        .iter()
        .flat_map(|root| walk(root, extra_exclusion))
        .collect();
    let actual = provider
        .search(SearchQuery {
            text: String::new(),
            max_results: usize::MAX,
            offset: 0,
        })
        .await
        .unwrap();
    assert_eq!(actual.total_count, Some(expected.len()));
    assert_eq!(
        actual
            .results
            .into_iter()
            .map(|row| (row.path, row.is_directory))
            .collect::<BTreeSet<_>>(),
        expected
    );
}

async fn restart(
    cache: &Path,
    roots: &[PathBuf],
    extra_exclusion: Option<&str>,
) -> Arc<FolderProvider> {
    let provider = provider(cache, roots, extra_exclusion);
    provider.initialize().await.unwrap();
    assert_walk(&provider, roots, extra_exclusion).await;
    provider
}

#[test]
#[ignore = "child fixture used by the NTFS persistence acceptance test"]
fn interrupt_snapshot_write_child() {
    let (cache, a, _) = fixture();
    assert_eq!(
        std::env::var("VELOCITY_NTFS_INTERRUPT_WRITER").as_deref(),
        Ok("yes")
    );
    let identity = Reader::open(&a)
        .unwrap()
        .checkpoint()
        .unwrap()
        .volume_identity;
    let target = cache_file(&cache, &identity).unwrap();
    let mut data = read_file(File::open(target).unwrap()).unwrap();
    data.checkpoint.journal_id ^= 1;
    let pending = stage_write(&cache, &data).unwrap();
    assert!(pending.0.exists());
    // Exit without running destructors after flushing the temporary file.
    std::process::exit(73);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the two disposable volume images and private cache created by the PowerShell runner"]
async fn persistent_snapshots_recover_against_fresh_walks() {
    let (cache, a, b) = fixture();
    let roots = [a.clone(), b.clone()];
    let baseline = restart(&cache, &roots, None).await;
    let checkpoint = coverage(&baseline, &a).ntfs.unwrap().checkpoint.clone();
    let target = cache_file(&cache, &checkpoint.volume_identity).unwrap();
    let original = fs::read(&target).unwrap();
    drop(baseline);

    let ordinary = restart(&cache, &roots, None).await;
    assert!(ordinary
        .status()
        .roots
        .iter()
        .all(|root| root.ntfs_restored));
    drop(ordinary);
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "ntfs::snapshot::disk::tests::interrupt_snapshot_write_child",
            "--exact",
            "--ignored",
        ])
        .env("VELOCITY_NTFS_INTERRUPT_WRITER", "yes")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(73));
    assert_eq!(fs::read(&target).unwrap(), original);
    assert!(fs::read_dir(&cache).unwrap().any(|entry| entry
        .unwrap()
        .path()
        .extension()
        .is_some_and(|ext| ext == "tmp")));

    let offline = a.join("fixture/alpha/offline-雪.txt");
    fs::rename(a.join("fixture/alpha/budget-shared.txt"), &offline).unwrap();
    let catch_up = restart(&cache, &roots, None).await;
    assert!(catch_up
        .status()
        .roots
        .iter()
        .all(|root| root.ntfs_restored));
    let latest = coverage(&catch_up, &a).ntfs.unwrap();
    assert!(latest.checkpoint.next_usn > checkpoint.next_usn);
    assert!(latest.file_reference(&offline).is_some());
    drop(catch_up);

    let limited = a.join("fixture/alpha");
    let trustee = std::env::var("VELOCITY_NTFS_TRUSTEE").unwrap();
    let acl = |args: &[&str]| {
        let status = Command::new("icacls.exe")
            .arg(&limited)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    };
    acl(&["/deny", &format!("{trustee}:(RD)")]);
    let denied = restart(&cache, &roots, None).await;
    assert!(coverage(&denied, &a)
        .ntfs
        .unwrap()
        .file_reference(&offline)
        .is_none());
    acl(&["/remove:d", &trustee]);
    let excluded = restart(&cache, &roots, Some("alpha")).await;
    assert!(excluded
        .status()
        .roots
        .iter()
        .all(|root| !root.ntfs_restored));
    assert!(coverage(&excluded, &a)
        .ntfs
        .unwrap()
        .file_reference(&offline)
        .is_none());
    let before_wrap = restart(&cache, &roots, None).await;
    let cursor = coverage(&before_wrap, &a).ntfs.unwrap().checkpoint.next_usn;
    let reader = Reader::open(&a).unwrap();
    for iteration in 0..20_000 {
        let churn = a.join("fixture/batch").join(format!(
            "journal-rollover-{iteration:05}-{}.tmp",
            "x".repeat(140)
        ));
        fs::write(&churn, "generate journal records").unwrap();
        fs::remove_file(churn).unwrap();
        if iteration % 512 == 511 && reader.journal().unwrap().FirstUsn > cursor {
            break;
        }
    }
    // NTFS trims on a filesystem checkpoint, which can happen after the burst
    // that grows the journal beyond its configured capacity.
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    let state = loop {
        let state = reader.journal().unwrap();
        if state.FirstUsn > cursor || std::time::Instant::now() >= deadline {
            break state;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let first = state.FirstUsn;
    assert!(
        first > cursor,
        "the real journal did not wrap past the saved cursor: {first} <= {cursor}, next={}, maximum={}, allocation={}",
        state.NextUsn,
        state.MaximumSize,
        state.AllocationDelta
    );
    assert_eq!(
        state.UsnJournalID,
        coverage(&before_wrap, &a)
            .ntfs
            .unwrap()
            .checkpoint
            .journal_id
    );
    println!("journal wrap advanced the first readable USN from before {cursor} to {first}");
    drop(reader);
    let wrapped = restart(&cache, &roots, None).await;
    assert!(!coverage(&wrapped, &a).ntfs_restored);
    assert!(coverage(&wrapped, &b).ntfs_restored);

    let mut previous_journal = read_file(File::open(&target).unwrap()).unwrap();
    previous_journal.checkpoint.journal_id ^= 1;
    let staged = stage_write(&cache, &previous_journal).unwrap();
    replace(&staged.0, &target).unwrap();
    let reset = restart(&cache, &roots, None).await;
    assert!(!coverage(&reset, &a).ntfs_restored);
    assert!(coverage(&reset, &b).ntfs_restored);

    let mut damaged = fs::read(&target).unwrap();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 1;
    fs::write(&target, damaged).unwrap();
    let recovering = provider(&cache, &roots, None);
    let task = tokio::spawn({
        let recovering = recovering.clone();
        async move {
            recovering.initialize().await.unwrap();
        }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = recovering.status();
            if status
                .roots
                .iter()
                .any(|root| root.root == b && root.ntfs_restored)
            {
                assert_eq!(
                    status
                        .roots
                        .iter()
                        .find(|root| root.root == a)
                        .unwrap()
                        .state,
                    RootState::Scanning
                );
                let page = recovering
                    .search(SearchQuery {
                        text: "budget-shared.txt".into(),
                        offset: 0,
                        max_results: 10,
                    })
                    .await
                    .unwrap();
                assert!(page.results.iter().any(|row| row.path.starts_with(&b)));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("healthy volume should be searchable before the corrupted volume rebuilds");
    task.await.unwrap();
    assert_walk(&recovering, &roots, None).await;
    assert!(!coverage(&recovering, &a).ntfs_restored);
    assert!(coverage(&recovering, &b).ntfs_restored);
    fs::write(
        cache.join("original-volume.txt"),
        checkpoint.volume_identity,
    )
    .unwrap();
    println!("restart, interrupted save, offline edits, ACL and exclusion changes, journal wrap, corrupt snapshot and independent recovery passed");
}

#[tokio::test]
#[ignore = "requires a new disposable volume mounted at the original volume's drive letter"]
async fn replacement_volume_does_not_load_the_old_index() {
    let (cache, a, b) = fixture();
    let previous_identity = fs::read_to_string(cache.join("original-volume.txt")).unwrap();
    let old_cache = cache_file(&cache, &previous_identity).unwrap();
    assert!(old_cache.exists());
    let provider = restart(&cache, &[a.clone(), b.clone()], None).await;
    let new = coverage(&provider, &a);
    assert!(!new.ntfs_restored);
    let actual = new.ntfs.unwrap();
    assert_ne!(actual.checkpoint.volume_identity, previous_identity);
    assert!(actual
        .file_reference(&a.join("fixture/alpha/replacement-only.txt"))
        .is_some());
    assert!(actual
        .file_reference(&a.join("fixture/alpha/offline-雪.txt"))
        .is_none());
    assert!(coverage(&provider, &b).ntfs_restored);
    println!("new volume at the original drive letter rebuilt independently");
}
