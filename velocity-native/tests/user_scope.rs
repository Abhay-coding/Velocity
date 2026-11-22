#![cfg(windows)]

use std::{collections::BTreeSet, fs, path::Path};

use velocity_core::{IndexProvider, SearchQuery};
use velocity_native::{FolderProvider, RootState};

fn query(text: &str, max_results: usize) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        offset: 0,
        max_results,
    }
}

#[tokio::test]
#[ignore = "requires the isolated fixture prepared by tools/verify-user-scope.ps1"]
async fn verify_windows_account_scope() {
    use std::{io::ErrorKind, process::Command};

    let fixture = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let settings = fs::read_to_string(fixture.join("scope-probe.json")).unwrap();
    let settings: serde_json::Value =
        serde_json::from_str(settings.trim_start_matches('\u{feff}')).unwrap();
    let identity = Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .unwrap();
    assert!(identity.status.success());
    let identity = String::from_utf8_lossy(&identity.stdout);
    let sid = identity
        .trim()
        .rsplit(',')
        .next()
        .unwrap()
        .trim_matches('"');
    assert!(
        Some(sid) == settings["expected_sid"].as_str(),
        "probe ran under an unexpected Windows account"
    );
    let can_read_private = settings["can_read_private"].as_bool().unwrap();

    let shared = fixture.join("shared");
    let public = shared.join("public");
    let private = shared.join("restricted-budget");
    let excluded = shared.join("excluded");
    let other = fixture.join("other");
    let missing = fixture.join("missing");
    let private_file = private.join("budget-private.txt");
    if can_read_private {
        assert!(fs::read_dir(&private).is_ok());
        assert!(fs::File::open(&private_file).is_ok());
    } else {
        assert_eq!(
            fs::read_dir(&private).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::File::open(&private_file).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
    }

    let provider = FolderProvider::new(
        vec![
            shared.clone(),
            public.clone(),
            private.clone(),
            excluded.clone(),
            other.clone(),
            missing.clone(),
        ],
        &["**/excluded/**".into()],
    )
    .unwrap();
    provider.initialize().await.unwrap();

    let snapshot = provider.search(query("", 100)).await.unwrap();
    let mut expected = BTreeSet::from([
        (public.clone(), true),
        (public.join("budget-shared.txt"), false),
        (other.join("budget-independent.txt"), false),
    ]);
    if can_read_private {
        expected.insert((private.clone(), true));
        expected.insert((private_file.clone(), false));
    }
    assert_eq!(snapshot.total_count, Some(expected.len()));
    assert_eq!(
        snapshot.results.len(),
        expected.len(),
        "duplicate or missing search results"
    );
    assert_eq!(
        snapshot
            .results
            .into_iter()
            .map(|row| (row.path, row.is_directory))
            .collect::<BTreeSet<_>>(),
        expected
    );

    let expected_budget = if can_read_private { 4 } else { 2 };
    let page = provider.search(query("budget", 100)).await.unwrap();
    let count = provider.search(query("budget", 0)).await.unwrap();
    assert_eq!(page.results.len(), expected_budget);
    assert_eq!(page.total_count, Some(expected_budget));
    assert_eq!(count.total_count, Some(expected_budget));
    assert!(count.results.is_empty());
    let private_matches = provider
        .search(query("budget-private.txt", 0))
        .await
        .unwrap();
    assert_eq!(
        private_matches.total_count,
        Some(usize::from(can_read_private))
    );

    let status = provider.status();
    let coverage = |path: &Path| status.roots.iter().find(|root| root.root == path).unwrap();
    assert_eq!(coverage(&shared).state, RootState::Ready);
    assert_eq!(coverage(&other).state, RootState::Ready);
    assert_eq!(coverage(&public).state, RootState::Skipped);
    assert_eq!(coverage(&excluded).state, RootState::Skipped);
    assert_eq!(coverage(&missing).state, RootState::Unavailable);
    assert_eq!(
        coverage(&private).state,
        if can_read_private {
            RootState::Skipped
        } else {
            RootState::AccessDenied
        }
    );
    assert_eq!(coverage(&private).entries, 0);
    assert_eq!(coverage(&shared).volume, coverage(&other).volume);
    let volume = coverage(&shared)
        .volume
        .as_ref()
        .expect("detect the fixture volume");
    assert_eq!(volume.filesystem, "NTFS");
    assert!(
        volume.identity.is_some(),
        "the fixture volume must have a stable GUID"
    );
    let receipt = if can_read_private {
        "owner-verified"
    } else {
        "observer-verified"
    };
    fs::write(fixture.join("scope-receipt.txt"), receipt).unwrap();
    println!(
        "can_read_private={can_read_private} visible_entries={} budget_matches={expected_budget}",
        expected.len()
    );
}
