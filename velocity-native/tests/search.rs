use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use velocity_core::{IndexProvider, SearchQuery, VelocityError};
use velocity_native::{FolderProvider, RootState};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("velocity-native-{}-{stamp}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, relative: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, relative).unwrap();
    }
}

async fn wait_for(mut predicate: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !predicate().await {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("folder monitor should settle");
}

#[cfg(windows)]
#[tokio::test]
async fn live_changes_and_reconnect_converge_independently() {
    let fixture = Fixture::new();
    fixture.file("live/old-folder/live-file.txt");
    let root = fixture.0.join("live");
    let other = Fixture::new();
    other.file("independent-file.txt");
    let provider = Arc::new(FolderProvider::new(vec![root.clone(), other.0.clone()], &[]).unwrap());
    let monitor = provider.start_monitor();
    wait_for(async || provider.status().revision > 0).await;
    fs::rename(root.join("old-folder"), root.join("new-folder")).unwrap();
    fs::hard_link(
        root.join("new-folder/live-file.txt"),
        root.join("live-link.txt"),
    )
    .unwrap();
    fs::remove_file(root.join("new-folder/live-file.txt")).unwrap();
    fs::write(root.join("new-folder/replacement-file.txt"), b"replacement").unwrap();
    wait_for(async || {
        let results = provider.search(query("", 0, 100)).await.unwrap();
        let actual: BTreeSet<_> = results.results.iter().map(|row| row.path.clone()).collect();
        actual
            == BTreeSet::from([
                root.join("live-link.txt"),
                root.join("new-folder"),
                root.join("new-folder/replacement-file.txt"),
                other.0.join("independent-file.txt"),
            ])
    })
    .await;

    fs::rename(&root, fixture.0.join("offline")).unwrap();
    wait_for(async || {
        provider
            .status()
            .roots
            .iter()
            .any(|entry| entry.root == root && entry.state == RootState::Unavailable)
    })
    .await;
    assert_eq!(
        provider
            .search(query("independent-file", 0, 100))
            .await
            .unwrap()
            .total_count,
        Some(1)
    );
    assert_eq!(
        provider
            .search(query("replacement-file", 0, 100))
            .await
            .unwrap()
            .total_count,
        Some(0)
    );
    fixture.file("offline/offline-change.txt");
    fs::rename(fixture.0.join("offline"), &root).unwrap();
    wait_for(async || {
        provider
            .search(query("offline-change", 0, 100))
            .await
            .unwrap()
            .total_count
            == Some(1)
    })
    .await;
    provider.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), monitor)
        .await
        .unwrap()
        .unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn retry_keeps_an_independent_snapshot_searchable() {
    let fixture = Fixture::new();
    fixture.file("healthy/published-name.txt");
    let roots = vec![fixture.0.join("healthy"), fixture.0.join("unavailable")];
    let provider = FolderProvider::new(roots, &[]).unwrap();
    provider.initialize().await.unwrap();
    assert_eq!(provider.status().indexed_entries, 1);
    assert!(provider.status().searchable);
    fixture.file("unavailable/recovered-name.txt");
    fs::rename(
        fixture.0.join("healthy/published-name.txt"),
        fixture.0.join("healthy/edited-name.txt"),
    )
    .unwrap();

    provider.retry_unavailable().await.unwrap();
    assert_eq!(provider.status().indexed_entries, 2);
    assert_eq!(
        provider
            .search(query("recovered-name", 0, 10))
            .await
            .unwrap()
            .total_count,
        Some(1)
    );
    assert_eq!(
        provider
            .search(query("published-name", 0, 10))
            .await
            .unwrap()
            .total_count,
        Some(1)
    );
    assert_eq!(
        provider
            .search(query("edited-name", 0, 10))
            .await
            .unwrap()
            .total_count,
        Some(0)
    );

    provider.reindex().await.unwrap();
    assert_eq!(
        provider
            .search(query("edited-name", 0, 10))
            .await
            .unwrap()
            .total_count,
        Some(1)
    );
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "uses the temporary SMB share created by tools/verify-folder-reconnect.ps1"]
async fn network_root_reconnects_and_reconciles_downtime() {
    let stage = PathBuf::from(std::env::var_os("VELOCITY_M8_STAGE").unwrap());
    let remote = PathBuf::from(std::env::var_os("VELOCITY_M8_SHARE").unwrap());
    let local = stage.join("local");
    let backing = stage.join("network");
    let provider = Arc::new(FolderProvider::new(vec![remote.clone(), local.clone()], &[]).unwrap());
    let monitor = provider.start_monitor();
    let healthy = local.join("still-searchable.txt");
    let old = remote.join("before-disconnect.txt");
    let state = |root: &Path| {
        provider
            .status()
            .roots
            .into_iter()
            .find(|item| item.root == root)
            .unwrap()
            .state
    };
    wait_for(async || state(&remote) == RootState::Ready && state(&local) == RootState::Ready)
        .await;
    assert!(matches_manifest(&provider, &[(old, false), (healthy.clone(), false)]).await);

    fs::write(stage.join("offline-request"), "").unwrap();
    wait_for(async || {
        assert_eq!(state(&local), RootState::Ready);
        assert_eq!(
            provider
                .search(query("still-searchable", 0, 10))
                .await
                .unwrap()
                .total_count,
            Some(1)
        );
        state(&remote) == RootState::Unavailable
    })
    .await;
    assert!(matches_manifest(&provider, &[(healthy.clone(), false)]).await);
    assert!(fs::read_dir(&remote).is_err());
    fs::remove_file(backing.join("before-disconnect.txt")).unwrap();
    fs::create_dir(backing.join("changed-offline")).unwrap();
    fs::write(
        backing.join("changed-offline/after-reconnect.txt"),
        "offline edit",
    )
    .unwrap();
    fs::write(stage.join("offline-observed"), "").unwrap();

    let updated = remote.join("changed-offline/after-reconnect.txt");
    let dir = remote.join("changed-offline");
    let recovered = [
        (healthy.clone(), false),
        (dir.clone(), true),
        (updated, false),
    ];
    wait_for(async || {
        assert_eq!(state(&local), RootState::Ready);
        assert_eq!(
            provider
                .search(query("still-searchable", 0, 10))
                .await
                .unwrap()
                .total_count,
            Some(1)
        );
        state(&remote) == RootState::Ready && matches_manifest(&provider, &recovered).await
    })
    .await;
    provider.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), monitor)
        .await
        .unwrap()
        .unwrap();

    fs::rename(
        backing.join("changed-offline/after-reconnect.txt"),
        backing.join("after-downtime.txt"),
    )
    .unwrap();
    fs::remove_dir(backing.join("changed-offline")).unwrap();
    let restarted = FolderProvider::new(vec![remote.clone(), local], &[]).unwrap();
    restarted.initialize().await.unwrap();
    assert!(
        matches_manifest(
            &restarted,
            &[(healthy, false), (remote.join("after-downtime.txt"), false)]
        )
        .await
    );
    println!("SMB root became unavailable, recovered offline changes, and reconciled app downtime; the independent root remained searchable");
}

#[cfg(windows)]
async fn matches_manifest(provider: &FolderProvider, manifest: &[(PathBuf, bool)]) -> bool {
    let page = provider
        .search(query("", 0, manifest.len() + 1))
        .await
        .unwrap();
    page.total_count == Some(manifest.len())
        && page
            .results
            .into_iter()
            .map(|row| (row.path, row.is_directory))
            .collect::<BTreeSet<_>>()
            == manifest.iter().cloned().collect()
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "applies and removes a deny ACL on an isolated test directory"]
async fn denied_subtree_is_absent_from_results_and_counts() {
    use std::process::Command;

    let fixture = Fixture::new();
    fixture.file("public-budget.txt");
    fixture.file("restricted/private-budget.txt");
    let restricted = fixture.0.join("restricted");
    let provider =
        Arc::new(FolderProvider::new(vec![fixture.0.clone(), restricted.clone()], &[]).unwrap());
    let monitor = provider.start_monitor();
    wait_for(async || {
        provider
            .search(query("budget", 0, 100))
            .await
            .ok()
            .and_then(|page| page.total_count)
            == Some(2)
    })
    .await;
    let whoami = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .unwrap();
    assert!(whoami.status.success());
    let output = String::from_utf8_lossy(&whoami.stdout);
    let sid = output.trim().rsplit(',').next().unwrap().trim_matches('"');
    assert!(sid.starts_with("S-1-"));
    let trustee = format!("*{sid}");
    let grant = format!("{trustee}:(RD)");
    let deny = Command::new("icacls")
        .arg(&restricted)
        .args(["/deny", &grant])
        .output()
        .unwrap();
    assert!(deny.status.success(), "{:?}", deny);

    let denied = fs::read_dir(&restricted).is_err();
    let reconciled = tokio::time::timeout(Duration::from_secs(10), async {
        while !provider
            .status()
            .roots
            .iter()
            .any(|root| root.root == restricted && root.state == RootState::AccessDenied)
        {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await;
    let found = provider.search(query("budget", 0, 100)).await;
    let restore = Command::new("icacls")
        .arg(&restricted)
        .args(["/remove:d", &trustee])
        .output()
        .unwrap();
    assert!(restore.status.success(), "{:?}", restore);
    reconciled.expect("access change should withdraw the restricted root");
    assert!(denied);
    let found = found.unwrap();
    assert_eq!(found.total_count, Some(1));
    assert_eq!(found.results[0].path, fixture.0.join("public-budget.txt"));
    wait_for(async || {
        provider
            .search(query("budget", 0, 100))
            .await
            .ok()
            .and_then(|page| page.total_count)
            == Some(2)
    })
    .await;
    provider.shutdown().await.unwrap();
    monitor.await.unwrap();
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn query(text: &str, offset: usize, max_results: usize) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        offset,
        max_results,
    }
}

fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .to_str()
        .unwrap()
        .replace('\\', "/")
}

#[tokio::test]
async fn scope_query_order_and_paging() {
    let fixture = Fixture::new();
    for path in [
        "alpha/Budget Draft.pdf",
        "beta/Budget Draft.pdf",
        "beta/予算 budget.pdf",
        "beta/Annual BUDGET.xlsx",
        "beta/budget archive/rare-7f3a.txt",
        "excluded/budget-hidden.pdf",
    ] {
        fixture.file(path);
    }
    let other = Fixture::new();
    other.file("budget-second-root.txt");
    let skipped = fixture.0.join("excluded");
    let missing = fixture.0.join("missing");
    let provider = FolderProvider::new(
        vec![
            fixture.0.join("beta"),
            fixture.0.clone(),
            other.0.clone(),
            missing.clone(),
            skipped,
        ],
        &["**/excluded/**".into()],
    )
    .unwrap();
    provider.initialize().await.unwrap();
    let status = provider.status();
    assert_eq!(
        status
            .roots
            .iter()
            .filter(|root| root.state == RootState::Ready)
            .count(),
        2
    );
    assert!(status.roots.iter().any(|root| root.skipped == 1));
    // A missing nested root is still reported even though its parent was indexed.
    assert!(status
        .roots
        .iter()
        .any(|root| root.root == missing && root.state == RootState::Unavailable));

    let all = provider.search(query("BuDgEt", 0, 100)).await.unwrap();
    assert_eq!(all.total_count, Some(6));
    assert!(!all.partial);
    let paths: Vec<_> = all
        .results
        .iter()
        .map(|result| result.path.clone())
        .collect();
    assert_eq!(paths.iter().collect::<BTreeSet<_>>().len(), 6);
    assert!(paths.contains(&other.0.join("budget-second-root.txt")));
    for offset in [0, 2, 4, 6, usize::MAX] {
        let page = provider.search(query("BuDgEt", offset, 2)).await.unwrap();
        assert_eq!(page.total_count, Some(6));
        assert_eq!(page.partial, offset < 4);
        let expected: Vec<_> = paths.iter().skip(offset).take(2).collect();
        assert_eq!(
            page.results.iter().map(|row| &row.path).collect::<Vec<_>>(),
            expected
        );
    }
    let count = provider.search(query("budget", 0, 0)).await.unwrap();
    assert_eq!(count.total_count, Some(6));
    assert!(count.results.is_empty() && count.partial);
    for (text, expected) in [
        (
            "ext:PDF budget",
            vec![
                "alpha/Budget Draft.pdf",
                "beta/Budget Draft.pdf",
                "beta/予算 budget.pdf",
            ],
        ),
        (
            "\"budget draft\"",
            vec!["alpha/Budget Draft.pdf", "beta/Budget Draft.pdf"],
        ),
        ("\"beta\\budget draft.pdf\"", vec!["beta/Budget Draft.pdf"]),
        ("rare-7f3a", vec!["beta/budget archive/rare-7f3a.txt"]),
        ("no-match-67e2", vec![]),
    ] {
        let found = provider.search(query(text, 0, 100)).await.unwrap();
        assert_eq!(found.total_count, Some(expected.len()), "{text}");
        assert_eq!(
            found
                .results
                .iter()
                .map(|row| relative(&row.path, &fixture.0))
                .collect::<Vec<_>>(),
            expected,
            "{text}"
        );
    }
    for invalid in ["content:secret", "ext:", "budget | archive", "\"budget"] {
        assert!(
            matches!(
                provider.search(query(invalid, 0, 100)).await,
                Err(VelocityError::InvalidQuery(_))
            ),
            "{invalid}"
        );
    }
}

#[tokio::test]
async fn warm_snapshot_search_uses_only_indexed_names() {
    let fixture = Fixture::new();
    fixture.file("old-search-name.txt");
    let provider = FolderProvider::new(vec![fixture.0.clone()], &[]).unwrap();
    provider.initialize().await.unwrap();
    provider.pause_indexing().await.unwrap();
    fs::rename(
        fixture.0.join("old-search-name.txt"),
        fixture.0.join("new-search-name.txt"),
    )
    .unwrap();
    let page = provider.search(query("old-search", 0, 10)).await.unwrap();
    assert_eq!(page.results[0].path, fixture.0.join("old-search-name.txt"));
    assert_eq!(page.results[0].size, None);
    assert_eq!(page.results[0].modified, None);
    provider.resume_indexing().await.unwrap();
    assert_eq!(
        provider
            .search(query("old-search", 0, 10))
            .await
            .unwrap()
            .total_count,
        Some(0)
    );
    assert_eq!(
        provider
            .search(query("new-search", 0, 10))
            .await
            .unwrap()
            .total_count,
        Some(1)
    );
}

#[tokio::test]
#[ignore = "requires a Windows fixture created with tools/search-fixture.ps1"]
async fn windows_fixture_matches_manifest() {
    let manifest_path =
        std::env::var_os("VELOCITY_FIXTURE_MANIFEST").expect("set VELOCITY_FIXTURE_MANIFEST");
    let json = fs::read_to_string(manifest_path).unwrap();
    let manifest: Value = serde_json::from_str(json.trim_start_matches('\u{feff}')).unwrap();
    let root = PathBuf::from(manifest["root"].as_str().unwrap());
    let provider = FolderProvider::new(vec![root.clone()], &["**/excluded/**".into()]).unwrap();
    provider.initialize().await.unwrap();
    let entries: BTreeMap<_, _> = manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["path"].as_str().unwrap().to_owned(),
                row["type"] == "directory",
            )
        })
        .collect();
    let result = provider.search(query("", 0, usize::MAX)).await.unwrap();
    assert_eq!(result.total_count, Some(entries.len()));
    assert_eq!(
        result
            .results
            .iter()
            .map(|row| (relative(&row.path, &root), row.is_directory))
            .collect::<BTreeMap<_, _>>(),
        entries
    );
    for case in manifest["queries"].as_array().unwrap() {
        let text = case["query"].as_str().unwrap();
        let expected: BTreeSet<_> = case["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| path.as_str().unwrap().to_owned())
            .collect();
        let result = provider.search(query(text, 0, 100)).await.unwrap();
        assert_eq!(result.total_count, Some(expected.len()), "{text}");
        assert_eq!(
            result
                .results
                .iter()
                .map(|row| relative(&row.path, &root))
                .collect::<BTreeSet<_>>(),
            expected,
            "{text}"
        );
    }
    let duplicate = provider.search(query("予算-budget", 0, 100)).await.unwrap();
    assert_eq!(duplicate.total_count, Some(2));
    for row in duplicate.results {
        assert_eq!(
            fs::read_to_string(&row.path).unwrap(),
            relative(&row.path, &root)
        );
    }
}
