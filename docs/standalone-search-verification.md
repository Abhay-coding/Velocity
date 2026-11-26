# Standalone search verification

## M1 reference fixture

Run `pwsh -NoProfile -File tools/search-fixture.ps1` to create and clean up a disposable local fixture and test the comparison with a deliberately missing file and duplicate row. Use a local Windows volume that supports hard links and junctions.

Use `-Action Create -Directory <new-path>` to keep the fixture for desktop or provider checks. The manifest sits next to the fixture root and lists the complete in-scope relative paths and types plus six expected query result sets. With `-Action Verify -Directory <root> -Actual <results.json>`, the verifier takes a JSON array of `{ "path": "relative/path", "type": "file" }` objects and prints missing and extra rows individually. The directory type is `directory`. `-Action Everything -Directory <root> -EverythingCli <es.exe>` runs the same query cases if Everything has indexed the fixture.

The baseline UI asks for at least two characters and debounces edits by 150 ms. Files and Folders filter the returned page. The Everything adapter requests name sorting (`-s`), and the UI preserves the provider's row order. It automatically selects the first visible row. Enter opens the selection, and Ctrl+Enter opens its parent folder. This behavior was recorded from the existing source.

## Milestone evidence

| Milestone | Evidence and remaining acceptance |
| --- | --- |
| M1 · Reference results | The self-test printed the exact intentionally missing and duplicate paths. The installed Everything CLI returned `IPC unavailable`, so there is no legacy comparison for this host. |
| M2 · Folder search | The Windows manifest matched the complete native result set and all six queries, including the long path, both hard links, and excluded and junction subtrees. A desktop test selected the second of two identical Unicode filenames by keyboard. Its distinctive Beta contents appeared in Notepad. |
| M3 · Queries and pages | Tests cover AND terms, quotes, case-insensitive names, paths, `ext:`, invalid operators, count-only mode, adjacent pages, and an out-of-range offset. Ordering is by the folded name, folded full path, original parent path, and original name. Exact totals cover the published scope. The Files and Folders controls filter the displayed page. |
| M4 · Typing path | Native search reads only the snapshot. A provider test renamed a file while indexing was paused and still found the indexed name with unknown metadata. The desktop acceptance run verified that the narrow result painted during a rebuild and that obsolete results never painted after the narrow edit. See the M4 procedure and measurements below. |
| M5 · Scope and access | The Windows acceptance run passed under two distinct account SIDs. The restricted subtree stayed out of the standard observer's results and counts, and the independent root remained searchable. Tests also cover overlapping roots, exclusions, stable NTFS volume identity, and live ACL revocation and recovery. |
| M6 · NTFS snapshot | Two disposable NTFS volumes passed the exact path and type comparison against independent walks. The opt-in reader captured each volume GUID, journal ID and starting USN before enumeration. It reconciled both hard-link names under the running account's access rules. A forced reader failure returned the same scoped results through folder indexing. |
| M7 · NTFS journal | Replay catches up from the initial scan position and tails the journal while search is open. The Windows acceptance suite compares live changes with independent walks and checks that the open desktop query refreshes after directory and file renames. |
| M8 · Folder changes | `ReadDirectoryChangesW` starts before the initial walk. Small file batches update affected entries. Overflow, directory moves, access changes and reconnects use a scoped walk. A real notification overflow converged to the expected manifest. A temporary SMB root recovered changes made while disconnected, and a separate local root stayed searchable. [Microsoft directory notification documentation](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw) |
| M9 · Persistence | Versioned per-volume snapshots and their journal cursors are saved together. A disposable-volume run verified restart, offline edits, an interrupted save, access and exclusion changes, journal wrap, corruption, and a different volume at the original drive letter. A healthy volume remained searchable during recovery. |
| M10 · Desktop controls | The desktop smoke opened the correct Unicode file and observed the active query refresh and an unavailable root becoming ready. Provider tests cover pause, resume, and monitor shutdown. The full keyboard, tray, hotkey and Retry acceptance run remains open. |
| M11 · Release performance | Both corpus sizes were exercised. See the benchmark results below for the budgets and the remaining measurements. |
| M12 · Standalone release | Native folder search is the default. Everything is available with the `everything` Cargo feature and a matching configuration setting. The default dependency graph and ZIP contents were inspected. Clean-VM installation and release sign-off remain open. |

The native syntax covers the fixture and the desktop's advertised filename and extension queries. Review saved searches that use OR, NOT, grouping, wildcards, macros, case or regex modifiers, extension lists, content or property predicates before migrating them. Unsupported expressions produce an error. [Everything search syntax](https://www.voidtools.com/support/everything/search_syntax/)

Run the locked workspace tests and the optional desktop build check with `cargo test --locked --workspace` and `cargo check --locked -p velocity-ui --features everything`. For Windows fixture acceptance, set `VELOCITY_FIXTURE_MANIFEST` to the manifest generated by `-Action Create`, then run `cargo test --locked -p velocity-native --test search -- --ignored`. This also exercises the isolated ACL test, which applies and removes a deny rule on a temporary directory.

## Release candidate

The x64 ZIP was built with Rust 1.97.1 from the working tree based on `b6ed6f4`. It contains `velocity-ui.exe` and `config.example.toml`. SHA-256 `97C012B4F42E18B63652FBD74AD4ACA45B9C127C75332B31DC2A90F7FE360DB2`. The default desktop dependency graph contains `velocity-native` and omits both the Everything adapter and experimental MFT parser. The optional Everything build passed its type check. The candidate links the MSVC runtime statically, and the executable import list contains only Windows system libraries. [Microsoft C++ deployment options](https://learn.microsoft.com/en-us/cpp/windows/deployment-in-visual-cpp)

Release decision remains pending. The development package is usable for folder search. Clean-VM release acceptance remains open.

The locked workspace suite passed 30 tests. The opt-in Windows tests for the junction fixture, live ACL changes and two-volume NTFS snapshot passed separately. Clippy passed with warnings denied for `velocity-native` and `velocity-ui`.

## M4 desktop acceptance

Run on a Windows desktop session with a local writable workspace.

```powershell
pwsh -NoProfile -File tools/typing-during-rebuild.ps1
```

The script builds Velocity in release mode with the opt-in `desktop-acceptance` feature, creates a disposable 200,000-file corpus, and launches the app with the FemtoVG renderer. The test types `report` and narrows it to `report-needle` after 35 ms while a native rebuild scans the same root. It checks every rendered frame after the narrow edit until the refreshed query settles. A trial fails if an obsolete result paints, metadata loads before the first page, or the narrow page first paints outside the rebuild window. The output reports each trial's first-paint latency and the nearest-rank p50 and p95. Adjust `-Entries` if the rebuild on your machine finishes before the first frame, and use `-Trials` to adjust the sample count.

On 21 November 2026, all 10 trials passed on the Windows reference host described below. First paint took **168.75 ms p50** and **188.92 ms p95**, including the 150 ms debounce. The expected filename painted during every rebuild and repainted after publication. The observer counted zero stale frames, the first rows showed unknown metadata, and all 21 provider page events reported zero filesystem reads in the search path. The workspace suite passed 30 tests, and Clippy passed with warnings denied for the desktop acceptance build.

First-paint samples in trial order, in milliseconds: 188.916, 166.173, 167.817, 168.582, 169.777, 169.348, 168.750, 168.695, 170.306, 172.327.

## M5 Windows account scope

Run this from a local NTFS checkout. Use an existing standard Windows test account with local sign-in rights. Enter its credentials in the local PowerShell prompt. [Microsoft secondary sign-in documentation](https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-createprocesswithlogonw)

```powershell
$observer = Get-Credential -UserName "$env:COMPUTERNAME\VelocityObserver" -Message 'Enter the existing Windows test account credentials'
./tools/verify-user-scope.ps1 -ObserverCredential $observer
```

Use the name of your existing test account in place of `VelocityObserver`. The runner probes the same fixture first as the owner and then as the observer. It verifies the SID and completion receipt for each process. The observer has read and execute access within the disposable fixture, a deny rule on the restricted subtree, and write access to the receipt. Cleanup removes the temporary fixture and its ACLs. The probe compares exact paths and types, paged and count-only totals, coverage for redundant, excluded, unavailable and denied roots, and the shared NTFS volume identity. [Microsoft ACL command documentation](https://learn.microsoft.com/windows-server/administration/windows-commands/icacls)

On 22 November 2026, the same-account harness and the two-account acceptance run both passed on the local NTFS workspace. The owner probe returned five entries and four `budget` matches. The observer probe ran under a distinct standard Windows account SID, returned three entries and two `budget` matches, and found zero matches for the restricted file. Both probes matched their complete expected path and type sets. The observer could still search the separate healthy root. Cleanup verified removal of the temporary fixture, account, and Windows profile.

## M6 two-volume NTFS acceptance

Run from the repository root in an elevated Windows PowerShell. The script creates two disposable 128 MiB virtual disks under `target`, verifies each disk identity before formatting, and detaches and deletes the images after the test. [Microsoft virtual disk mounting documentation](https://learn.microsoft.com/powershell/module/storage/mount-diskimage?view=windowsserver2025-ps)

```powershell
./tools/verify-ntfs-snapshot.ps1
```

On 23 November 2026, each volume returned 86 entries across 16 enumeration batches and matched the independent walk exactly. Both hard-link names shared a file reference and had distinct parent references. Identical relative paths stayed searchable on both volume identities. Denied, excluded and junction paths stayed outside the results and counts. Folder fallback matched the same walks after a forced snapshot failure. [Microsoft enumeration cursor documentation](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ns-winioctl-mft_enum_data_v0)

## M7 live NTFS acceptance

Run from the repository root in an elevated Windows PowerShell session on an interactive desktop. The runner uses the disposable M6 volumes. [Microsoft change journal read documentation](https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_read_usn_journal)

```powershell
./tools/verify-ntfs-snapshot.ps1 -Journal -Desktop
```

The startup test renames a file during the initial scan. It then reads again from the same checkpoint twice and compares the resulting projections and cursors. The live test checks exact paths, types and counts after removing one hard link, replacing a file at the same path, moving a file between directories, renaming a directory with matching descendants, and revoking and restoring directory listing access. It also verifies that content-only changes advance the journal position without refreshing the result list. The desktop test keeps one query open and waits for the old path to disappear and the new path to appear after each rename.

On 23 November 2026, the snapshot, startup replay, live journal and desktop checks passed. The active query painted the initial result and refreshed after both renames. Both virtual disks were detached and deleted after the run. The locked workspace suite passed all 30 regular tests, and Clippy passed with warnings denied, including the desktop acceptance build.

Ordinary file events refresh the affected directories where possible. Directory moves, hard links, permission changes and ambiguous batches can require a fresh scoped directory walk. The journal cursor and searchable entries publish together. NTFS replay throughput is still part of M11.

## M8 folder overflow and network reconnect acceptance

On Windows, run the regular test to force the watcher to overflow during a burst of file creates, moves and deletes. The test observes the kernel overflow and verifies the complete published path and type manifest and exact count. [Microsoft directory notification documentation](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-readdirectorychangesw)

```powershell
cargo test --locked -p velocity-native overflow_during_creates_moves_and_deletes_converges_to_manifest
```

Run the SMB test from an elevated Windows PowerShell with the Windows Server service running. The script creates a temporary share for synthetic data, gives the signed-in account read access, and removes the share and fixture afterward. The test connects through `localhost`. [Microsoft SMB share documentation](https://learn.microsoft.com/en-us/powershell/module/smbshare/new-smbshare)

```powershell
./tools/verify-folder-reconnect.ps1
```

On 24 November 2026, both tests passed. The SMB root became unavailable and its old results disappeared. Reconnecting recovered the changed files while the independent local root stayed ready and searchable. Restarting the provider also picked up changes made while it was stopped.

## M9 NTFS persistence and recovery acceptance

Run from the repository root in elevated Windows PowerShell. The runner creates three disposable virtual disks and a private cache under `target`. It verifies disk identities before formatting or reusing a drive letter and deletes the images afterward. [Microsoft virtual disk mounting documentation](https://learn.microsoft.com/powershell/module/storage/mount-diskimage?view=windowsserver2025-ps)

```powershell
./tools/verify-ntfs-snapshot.ps1 -Persistence
```

On 26 November 2026, each recovered index matched an independent directory walk, including its exact count. A child process exited after flushing a temporary save and before replacing the committed file. Restart kept the old file and replayed offline edits. A real NTFS journal wrap moved the first readable USN to 19,922,944, past the saved cursor of 29,744. Tests also rejected a mismatched journal ID and a damaged checksum. The healthy volume returned results while the damaged volume was still rebuilding. A third disk mounted at the original drive letter built its own snapshot under its distinct volume identity. [Microsoft journal capacity and trimming documentation](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/fsutil-usn)

Cache restores check the current directory visibility before serving results, including when the journal has no new records. Restore performance at the release corpus sizes remains part of M11.

## Release benchmark

Run from the repository root on Windows. Use a writable local volume with room for the disposable corpus.

```powershell
New-Item -ItemType Directory -Force target | Out-Null
$corpusPath = (Resolve-Path target).Path
cargo run --locked --release -p velocity-native --example search_bench -- $corpusPath 100000
cargo run --locked --release -p velocity-native --example search_bench -- $corpusPath 1000000
```

Reference host used on 20 November 2026, India time. Windows 26H2 build `26300.9974`, Intel Core i7-13650HX with 20 logical processors, and an NTFS volume on a WD PC SN560 NVMe SSD. The synthetic corpus contains real empty files and directories. The provider returns up to 100 rows including an exact count, with metadata lookup disabled. Each query class uses five warm-ups and 100 repetitions. Change measurements use three trials per operation. The initial folder build starts with an empty Velocity index after corpus generation. The OS disk cache may still contain the generated directory entries.

| Measurement | 100,000 entries | 1,000,000 entries |
| --- | ---: | ---: |
| Common substring, p50 / p95 ms | 1.86 / 2.63 | 28.99 / 37.51 |
| Rare substring, p50 / p95 ms | 1.71 / 2.24 | 26.61 / 29.32 |
| No matches, p50 / p95 ms | 1.74 / 2.79 | 21.27 / 31.30 |
| Quoted phrase, p50 / p95 ms | 1.73 / 2.09 | 20.93 / 31.26 |
| Path term, p50 / p95 ms | 2.07 / 2.52 | 27.88 / 46.45 |
| Extension and term, p50 / p95 ms | 1.47 / 1.85 | 17.21 / 26.52 |
| File creation, p50 / p95 ms | 93.30 / 101.69 | 227.83 / 259.28 |
| File move and rename, p50 / p95 ms | 110.56 / 132.92 | 248.15 / 255.12 |
| File deletion, p50 / p95 ms | 105.50 / 106.14 | 225.07 / 240.53 |
| Initial folder build | 256 ms | 2,612 ms |
| Warm folder restart and rescan | 210 ms | 2,512 ms |
| Working set after initial build | 40.6 MiB | 351.4 MiB |
| Peak working set through initial build | 40.6 MiB | 351.4 MiB |
| Private bytes after initial build | 38.4 MiB | 358.9 MiB |

All six provider query classes met the 50 ms p95 target at one million entries. The sampled ordinary file changes met the 1 second target. Both full-corpus comparisons found zero missing, stale, duplicate or out-of-scope paths after catch-up. The initial runs that rescanned whole roots after every notification took several seconds to show file changes at one million entries. The final run uses bounded file updates and falls back to a scoped walk for directory or access changes. The full desktop first-paint benchmark at the release corpus sizes, privileged NTFS timings, persisted snapshot size and load time, and an Everything timing comparison remain open.

[Raw 100,000-entry measurements](benchmarks/2026-11-20-folder-100000.txt) · [Raw 1,000,000-entry measurements](benchmarks/2026-11-20-folder-1000000.txt)
