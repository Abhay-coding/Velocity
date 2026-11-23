#![cfg(windows)]

use std::{
    collections::BTreeSet,
    fs, io,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};
use velocity_core::{IndexProvider, SearchQuery};
use velocity_native::{FolderProvider, RootState};

fn walk(root: &Path) -> io::Result<BTreeSet<(PathBuf, bool)>> {
    let mut expected = BTreeSet::new();
    let mut pending = vec![fs::read_dir(root)?];
    while let Some(directory) = pending.pop() {
        for child in directory {
            let child = child?;
            let name = child.file_name();
            if ["System Volume Information", "$RECYCLE.BIN", "excluded"]
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
            {
                continue;
            }
            let path = child.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_attributes() & 0x400 != 0 {
                continue;
            }
            if metadata.is_dir() {
                match fs::read_dir(&path) {
                    Ok(entries) => pending.push(entries),
                    Err(error) if error.kind() == io::ErrorKind::PermissionDenied => continue,
                    Err(error) => return Err(error),
                }
            }
            expected.insert((path, metadata.is_dir()));
        }
    }
    Ok(expected)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a disposable NTFS volume and its fixture SID"]
async fn tails_ntfs_journal_and_matches_a_fresh_walk() {
    let root = PathBuf::from(std::env::var_os("VELOCITY_NTFS_VOLUME_A").unwrap());
    let trustee = std::env::var("VELOCITY_NTFS_TRUSTEE").unwrap();
    let exclusions = [
        "**/System Volume Information/**".into(),
        "**/$RECYCLE.BIN/**".into(),
        "**/excluded/**".into(),
    ];
    let provider = Arc::new(
        FolderProvider::new(vec![root.clone()], &exclusions)
            .unwrap()
            .with_ntfs_snapshots(1024),
    );
    let monitor = provider.start_monitor();
    let initial = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(snapshot) = provider.status().roots[0].ntfs.clone() {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("startup must publish an NTFS snapshot");
    let parent = root.join("fixture/alpha");
    let replaced = parent.join("budget-shared.txt");
    let alias = root.join("fixture/beta/budget-alias.txt");
    let old_id = initial.file_reference(&replaced).unwrap().0;
    assert_eq!(initial.file_reference(&alias).unwrap().0, old_id);
    fs::remove_file(&alias).unwrap();
    let updated = wait_for_walk(&provider, &root, &initial).await;
    assert!(updated.file_reference(&alias).is_none());
    assert_eq!(updated.file_reference(&replaced).unwrap().0, old_id);

    let replacement = parent.join("replacement.tmp");
    fs::write(&replacement, "replacement").unwrap();
    fs::remove_file(&replaced).unwrap();
    fs::rename(&replacement, &replaced).unwrap();
    let updated = wait_for_walk(&provider, &root, &updated).await;
    assert_ne!(updated.file_reference(&replaced).unwrap().0, old_id);

    fs::rename(&replaced, &alias).unwrap();
    let updated = wait_for_walk(&provider, &root, &updated).await;
    assert!(updated.file_reference(&replaced).is_none());
    assert!(updated.file_reference(&alias).is_some());
    fs::rename(&alias, &replaced).unwrap();
    let updated = wait_for_walk(&provider, &root, &updated).await;
    assert!(updated.file_reference(&alias).is_none());
    assert!(updated.file_reference(&replaced).is_some());

    let moved = root.join("fixture/moved");
    fs::rename(&parent, &moved).unwrap();
    let updated = wait_for_walk(&provider, &root, &updated).await;
    let moved_file = moved.join("budget-shared.txt");
    assert!(updated.file_reference(&replaced).is_none());
    assert!(updated.file_reference(&moved_file).is_some());
    let result = Command::new("icacls.exe")
        .arg(&moved)
        .arg("/deny")
        .arg(format!("{trustee}:(RD)"))
        .output()
        .unwrap();
    assert!(result.status.success());
    let updated = wait_for_walk(&provider, &root, &updated).await;
    assert!(updated.file_reference(&moved_file).is_none());
    let result = Command::new("icacls.exe")
        .arg(&moved)
        .arg("/remove:d")
        .arg(trustee)
        .output()
        .unwrap();
    assert!(result.status.success());
    let updated = wait_for_walk(&provider, &root, &updated).await;
    assert!(updated.file_reference(&moved_file).is_some());

    let revision = provider.status().revision;
    let mut overwritten = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&moved_file)
        .unwrap();
    std::io::Write::write_all(&mut overwritten, b"content only").unwrap();
    drop(overwritten);
    wait_for_walk(&provider, &root, &updated).await;
    assert_eq!(provider.status().revision, revision);
    fs::rename(moved, parent).unwrap();
    provider.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), monitor)
        .await
        .unwrap()
        .unwrap();
    println!("live journal replacement, hard-link removal, directory move, ACL removal and recovery passed");
}

async fn wait_for_walk(
    provider: &FolderProvider,
    root: &Path,
    previous: &velocity_native::ntfs::ScopedSnapshot,
) -> Arc<velocity_native::ntfs::ScopedSnapshot> {
    let expected = walk(root).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = provider.status();
            if let Some(snapshot) = &status.roots[0].ntfs {
                assert_eq!(
                    snapshot.enumeration_start_usn, previous.enumeration_start_usn,
                    "expected replay on the original snapshot"
                );
                assert_eq!(
                    snapshot.checkpoint.journal_id,
                    previous.checkpoint.journal_id
                );
                if snapshot.checkpoint.next_usn > previous.checkpoint.next_usn {
                    let page = provider.search(query("")).await.unwrap();
                    let actual = page
                        .results
                        .into_iter()
                        .map(|row| (row.path, row.is_directory))
                        .collect::<BTreeSet<_>>();
                    if actual == expected {
                        assert_eq!(page.total_count, Some(expected.len()));
                        return snapshot.clone();
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("journal projection did not converge to the independent directory walk")
}

fn query(text: &str) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        max_results: usize::MAX,
        offset: 0,
    }
}

#[tokio::test]
#[ignore = "requires the two disposable NTFS volumes prepared by tools/verify-ntfs-snapshot.ps1"]
async fn scoped_ntfs_snapshots_match_independent_walks() {
    let volumes: Vec<PathBuf> = ["VELOCITY_NTFS_VOLUME_A", "VELOCITY_NTFS_VOLUME_B"]
        .into_iter()
        .map(|key| {
            std::env::var_os(key)
                .expect("set both disposable volume roots")
                .into()
        })
        .collect();
    let mut expected = BTreeSet::new();
    for root in &volumes {
        assert_eq!(
            fs::read_dir(root.join("fixture/restricted"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        expected.extend(walk(root).unwrap());
    }
    let exclusions = [
        "**/System Volume Information/**".into(),
        "**/$RECYCLE.BIN/**".into(),
        "**/excluded/**".into(),
    ];
    let provider = FolderProvider::new(volumes.clone(), &exclusions)
        .unwrap()
        .with_ntfs_snapshots(1024);
    provider.initialize().await.unwrap();

    let status = provider.status();
    let mut identities = BTreeSet::new();
    for root in &status.roots {
        assert_eq!(root.state, RootState::Ready);
        let snapshot = root.ntfs.as_ref().unwrap_or_else(|| {
            panic!(
                "NTFS snapshot fell back for {}: {:?}",
                root.root.display(),
                root.detail
            )
        });
        assert!(identities.insert(snapshot.checkpoint.volume_identity.clone()));
        assert_ne!(snapshot.checkpoint.journal_id, 0);
        assert!(snapshot.checkpoint.next_usn > 0);
        assert!(snapshot.enumeration_batches > 1);
        assert!(snapshot.hard_link_aliases >= 1);
        let (first, first_parent) = snapshot
            .file_reference(&root.root.join("fixture/alpha/budget-shared.txt"))
            .unwrap();
        let (second, second_parent) = snapshot
            .file_reference(&root.root.join("fixture/beta/budget-alias.txt"))
            .unwrap();
        assert_eq!(first, second);
        assert_ne!(first_parent, second_parent);
        assert!(root.skipped >= 3);
        println!(
            "{} batches={} aliases={} scoped_entries={}",
            root.root.display(),
            snapshot.enumeration_batches,
            snapshot.hard_link_aliases,
            root.entries
        );
    }
    assert_eq!(identities.len(), 2);
    let actual = provider.search(query("")).await.unwrap();
    assert_eq!(actual.total_count, Some(expected.len()));
    assert_eq!(actual.results.len(), expected.len());
    assert_eq!(
        actual
            .results
            .into_iter()
            .map(|row| (row.path, row.is_directory))
            .collect::<BTreeSet<_>>(),
        expected
    );

    for name in ["budget-shared.txt", "budget-alias.txt"] {
        let page = provider.search(query(name)).await.unwrap();
        assert_eq!(page.total_count, Some(2));
        assert_eq!(
            page.results
                .into_iter()
                .map(|row| row.path)
                .collect::<BTreeSet<_>>(),
            volumes
                .iter()
                .map(|root| {
                    root.join("fixture")
                        .join(if name == "budget-shared.txt" {
                            "alpha"
                        } else {
                            "beta"
                        })
                        .join(name)
                })
                .collect()
        );
    }
    for name in ["budget-private.txt", "budget-excluded.txt"] {
        assert_eq!(
            provider.search(query(name)).await.unwrap().total_count,
            Some(0)
        );
    }

    let fallback = FolderProvider::new(volumes, &exclusions)
        .unwrap()
        .with_ntfs_snapshots(0);
    fallback.initialize().await.unwrap();
    assert!(fallback.status().roots.iter().all(|root| {
        root.state == RootState::Ready && root.ntfs.is_none() && root.detail.is_some()
    }));
    let page = fallback.search(query("")).await.unwrap();
    assert_eq!(page.total_count, Some(expected.len()));
    assert_eq!(
        page.results
            .into_iter()
            .map(|row| (row.path, row.is_directory))
            .collect::<BTreeSet<_>>(),
        expected
    );
}
