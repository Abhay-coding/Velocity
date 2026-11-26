# Release search benchmark · 26 November 2026

The reference host ran Windows 11 Home build 26300 with a Core i7-13650HX, 20 logical processors, 16 GiB RAM and a WD PC SN560 NVMe SSD. The release binaries used Rust 1.97.1. The host-drive baseline used commit `0290ef7` with the M11 measurement harness. Raw records include the binary SHA-256 hashes.

Each corpus contains exactly 100,000 or 1,000,000 entries, including group directories and empty files with generated names. The query benchmark records 100 samples per class after five warm-ups. It returns up to 100 rows with an exact count and leaves metadata loading off. File creation, rename, move and deletion each have 20 trials. The desktop samples time the last edit through the first rendered result, including the 150 ms debounce. The live phase creates and removes files while typing. The initial build starts with an empty Velocity index, and the Windows disk cache can contain pages from corpus generation.

## Folder search on the host drive

| Measurement | 100,000 entries | 1,000,000 entries |
| --- | ---: | ---: |
| Common substring, p50 / p95 ms | 2.10 / 5.77 | 23.09 / 26.18 |
| Rare substring, p50 / p95 ms | 2.11 / 4.87 | 28.02 / 39.41 |
| No matches, p50 / p95 ms | 1.65 / 5.55 | 25.91 / 33.45 |
| Quoted phrase, p50 / p95 ms | 2.99 / 4.34 | 22.82 / 29.39 |
| Path term, p50 / p95 ms | 4.25 / 8.16 | 30.78 / 36.82 |
| Extension and term, p50 / p95 ms | 2.65 / 5.98 | 15.26 / 18.05 |
| Desktop idle, p50 / p95 ms | 167.99 / 179.21 | 192.02 / 208.22 |
| Desktop during changes, p50 / p95 ms | 168.53 / 180.56 | 191.66 / 207.10 |
| Create visibility, p50 / p95 ms | 102.47 / 120.32 | 140.71 / 202.39 |
| Rename visibility, p50 / p95 ms | 93.83 / 128.69 | 187.16 / 292.12 |
| Move visibility, p50 / p95 ms | 91.30 / 122.45 | 166.65 / 254.59 |
| Delete visibility, p50 / p95 ms | 96.37 / 126.20 | 140.57 / 214.18 |
| Initial folder build | 239 ms | 7,263 ms |
| Warm folder restart | 208 ms | 2,727 ms |
| Warm folder restart with one offline edit | 208 ms | 2,991 ms |
| Initial peak working set | 42.7 MiB | 382.6 MiB |
| Working set after the warm restart | 43.0 MiB | 384.9 MiB |
| Private bytes after the warm restart | 40.5 MiB | 398.5 MiB |

The full path and directory-type comparisons found zero missing, extra or duplicate results. Both desktop phases recorded zero stale frames. [100,000-entry raw samples](2026-11-26-folder-100000.txt) · [1,000,000-entry raw samples](2026-11-26-folder-1000000.txt)

## Whole-volume baseline

The first disposable-volume run found a different bottleneck. Protected directories at the volume root caused ordinary edits nearby to take the scoped-walk path. NTFS also rewrote the entire JSON snapshot synchronously after each visible edit. At one million entries, NTFS built its initial index in 6.25 s and the slowest query class was 29.28 ms p95. The four change classes ranged from 5.01 to 7.78 s p95. The JSON snapshot occupied 179,853,523 bytes and a warm restart took 4.75 s. The whole-volume folder backend also missed the visibility budget, with rename reaching 9.01 s p95 during the overlapping host-drive run. [Whole-volume baseline samples](2026-11-26-volume-baseline.txt)

The follow-up keeps ordinary file updates scoped to the affected directories when neighboring skipped entries can be verified. It coalesces live snapshot writes off the update path and writes a compact binary snapshot. Restart still validates the checksum, the saved journal position, and the current directory scope. Existing JSON snapshots remain readable.

## Whole-volume run with scoped updates and compact snapshots

| Measurement | Folder 100,000 | Folder 1,000,000 | NTFS 100,000 | NTFS 1,000,000 |
| --- | ---: | ---: | ---: | ---: |
| Common substring, p50 / p95 ms | 1.85 / 2.83 | 24.33 / 39.98 | 2.76 / 5.23 | 25.44 / 42.67 |
| Rare substring, p50 / p95 ms | 2.90 / 7.42 | 22.69 / 25.04 | 2.50 / 6.66 | 31.57 / 53.18 |
| No matches, p50 / p95 ms | 4.64 / 10.70 | 20.00 / 28.50 | 2.71 / 6.95 | 22.68 / 43.05 |
| Quoted phrase, p50 / p95 ms | 2.09 / 8.18 | 20.04 / 26.95 | 2.15 / 4.86 | 20.11 / 29.66 |
| Path term, p50 / p95 ms | 2.42 / 6.61 | 23.29 / 31.29 | 3.38 / 8.47 | 24.03 / 28.87 |
| Extension and term, p50 / p95 ms | 2.12 / 4.14 | 15.93 / 31.39 | 1.36 / 2.36 | 15.94 / 24.22 |
| Create visibility, p50 / p95 ms | 106.35 / 123.32 | 174.53 / 219.19 | 120.78 / 141.25 | 508.43 / 635.97 |
| Rename visibility, p50 / p95 ms | 93.31 / 129.46 | 208.00 / 253.95 | 126.72 / 164.36 | 599.24 / 687.54 |
| Move visibility, p50 / p95 ms | 89.04 / 119.72 | 173.18 / 272.67 | 261.40 / 340.55 | 2,031.35 / 2,471.22 |
| Delete visibility, p50 / p95 ms | 82.55 / 108.67 | 156.35 / 207.39 | 264.90 / 326.98 | 1,960.55 / 2,373.51 |
| Initial index, ms | 202 | 1,723 | 596 | 4,886 |
| Warm restart, ms | 181 | 1,449 | 793 | 4,730 |
| Warm restart with one offline edit, ms | 180 | 1,441 | 490 | 5,494 |
| Initial working set / peak / private, MiB | 32.8 / 32.8 / 30.8 | 274.6 / 274.6 / 282.3 | 37.3 / 50.1 / 41.1 | 303.6 / 509.6 / 375.0 |
| Persisted snapshot bytes per entry | — | — | 45.99 | 47.49 |

The one-million-entry snapshot shrank to 47,489,730 bytes, 74% below the JSON baseline. All four path comparisons had zero missing, extra or duplicate results. In folder mode, desktop live-update first paint was 178.71 ms p95 at 100,000 entries and 203.22 ms p95 at one million entries. Both desktop phases had zero stale frames. [Scoped-update run and raw desktop samples](2026-11-26-volume-tuned.txt)

## Final million-entry NTFS retest

The final build keeps the snapshot in search order, merges ordinary file changes into that order, and reuses cached rows after checking their paths, types and file references through the authorized directory walk. It also reads the checksummed snapshot into a buffer once. The final run used a fresh disposable volume with the same size, corpus, query and change workloads. Its build was based on `0290ef7` plus the M11 changes, and the raw record includes the executable hash. [Final million-entry NTFS raw samples](2026-11-26-ntfs-final-1000000.txt)

| Measurement | Final NTFS, 1,000,000 entries |
| --- | ---: |
| Common substring, p50 / p95 ms | 24.34 / 27.13 |
| Rare substring, p50 / p95 ms | 24.16 / 26.11 |
| No matches, p50 / p95 ms | 21.45 / 30.40 |
| Quoted phrase, p50 / p95 ms | 21.10 / 28.65 |
| Path term, p50 / p95 ms | 27.61 / 36.35 |
| Extension and term, p50 / p95 ms | 20.00 / 27.19 |
| Create visibility, p50 / p95 ms | 139.34 / 214.95 |
| Rename visibility, p50 / p95 ms | 194.83 / 207.95 |
| Move visibility, p50 / p95 ms | 158.80 / 224.58 |
| Delete visibility, p50 / p95 ms | 140.34 / 228.12 |
| Initial index ready / snapshot committed | 3,977 / 4,687 ms |
| Warm restart / restart with one offline edit | 2,996 / 2,413 ms |
| Snapshot decoding and entry construction, quiet / offline edit | 689 / 736 ms |
| Journal reads during restore, quiet / offline edit | <1 / <1 ms |
| Initial working set / peak / private | 304.4 / 509.3 / 377.3 MiB |
| Working set / private bytes after warm restart | 303.2 / 313.6 MiB |
| Snapshot size | 47,489,732 bytes (47.49 bytes per entry) |

Warm query sampling began after the initial snapshot committed. The first optimized run had a 53.18 ms p95 for the rare substring, so the final run profiled a second 100-query sweep. It measured 25.87 ms wall-clock p95 and 31.25 ms thread CPU p95 at the Windows timer resolution. The results support keeping the existing substring matcher. [Windows thread timing API](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getthreadtimes)

The retest met the cold build, warm restart, query and change-visibility budgets on the reference host. The measured quiet restart was 4 ms below its 3 s target. The final path and type comparison found zero missing, stale, duplicate or out-of-scope entries. Windows acceptance also repeated journal replay, recovery after an interrupted save, journal expiry, permission changes and volume replacement.

The local Everything CLI reported that its IPC window was unavailable. [Everything CLI requirements](https://www.voidtools.com/support/everything/command_line_interface/)
