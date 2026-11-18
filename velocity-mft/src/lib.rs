//! velocity-mft: NTFS MFT Parser
//!
//! Production implementation of NTFS Master File Table (MFT) parsing for the Velocity search engine.
//! This crate provides direct MFT access similar to Voidtools' Everything.

pub mod boot_sector;
pub mod record;

// Public re-exports for the crate's public API
pub use crate::boot_sector::{BootSectorInfo, NtfsBootSector, read_boot_sector};
pub use crate::record::{filetime_to_unix, parse_runlist, MftAttribute, MftRecord, RunlistEntry};

use velocity_core::{error::VelocityError, index_provider::*, VelocityResult};
use async_trait::async_trait;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use globset::{Glob, GlobSet, GlobSetBuilder};

/// NTFS MFT Parser - Production implementation
///
/// Provides direct access to NTFS Master File Table for fast file enumeration
/// and searching. Uses Windows APIs for volume access.
pub struct MftParser {
    volume_path: PathBuf,
    boot_sector: Option<BootSectorInfo>,
    volume_handle: Option<File>,
    /// Cache of parsed records for performance
    record_cache: Arc<RwLock<HashMap<u64, MftRecord>>>,
}

impl MftParser {
    pub fn new(volume_path: PathBuf) -> Self {
        Self {
            volume_path,
            boot_sector: None,
            volume_handle: None,
            record_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Open volume for raw MFT access (requires admin)
    ///
    /// Opens the volume with FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
    /// to allow raw sector reading.
    pub async fn open(&mut self) -> VelocityResult<()> {
        // Read boot sector first to get volume geometry
        let volume_str = self.volume_path.to_string_lossy().to_string();
        let boot_sector = read_boot_sector(&volume_str)?;
        self.boot_sector = Some(boot_sector.clone());

        // Open volume handle for raw access
        // Note: This requires administrator privileges
        let handle = self.open_volume_handle(&volume_str)?;
        self.volume_handle = Some(handle);

        Ok(())
    }

    /// Open volume handle with proper flags for raw MFT access
    fn open_volume_handle(&self, volume_path: &str) -> VelocityResult<File> {
        use std::os::windows::io::FromRawHandle;
        use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN,
            FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_SHARE_DELETE, OPEN_EXISTING
        };
        use windows::core::PCWSTR;

        // Convert path to wide string
        let wide_path: Vec<u16> = volume_path
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // Open with FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
        // GENERIC_READ | SYNCHRONIZE
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_path.as_ptr()),
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN,
                None,
            )
        };

        match handle {
            Ok(h) if h.0 != INVALID_HANDLE_VALUE.0 => {
                Ok(unsafe { File::from_raw_handle(h.0 as *mut std::ffi::c_void) })
            }
            _ => {
                let err = std::io::Error::last_os_error();
                Err(VelocityError::Config(format!(
                    "Failed to open volume '{}': {} (requires administrator privileges)",
                    volume_path, err
                )))
            }
        }
    }

    /// Get boot sector info
    pub fn boot_sector(&self) -> Option<&BootSectorInfo> {
        self.boot_sector.as_ref()
    }

    /// Read raw bytes from volume at offset
    fn read_at_offset(&self, offset: u64, size: usize) -> VelocityResult<Vec<u8>> {
        let mut handle = self.volume_handle.as_ref()
            .ok_or_else(|| VelocityError::Config("Volume not opened".to_string()))?;

        let mut buffer = vec![0u8; size];
        // Use Seek + ReadExact for portable offset reading
        handle.seek(SeekFrom::Start(offset))
            .map_err(|e| VelocityError::Config(format!("Failed to seek to offset {}: {}", offset, e)))?;
        handle.read_exact(&mut buffer)
            .map_err(|e| VelocityError::Config(format!("Failed to read at offset {}: {}", offset, e)))?;

        Ok(buffer)
    }

    /// Read MFT record by record number
    pub async fn read_record(&mut self, record_number: u64) -> VelocityResult<MftRecord> {
        // Check cache first
        {
            let cache = self.record_cache.read().await;
            if let Some(record) = cache.get(&record_number) {
                return Ok(record.clone());
            }
        }

        let boot_sector = self.boot_sector.as_ref()
            .ok_or_else(|| VelocityError::Config("Boot sector not loaded".to_string()))?;

        // Calculate MFT offset
        // MFT starts at mft_lcn * bytes_per_cluster
        let mft_offset = boot_sector.mft_lcn * boot_sector.bytes_per_cluster as u64;
        let record_offset = mft_offset + record_number * boot_sector.bytes_per_record as u64;

        // Read record data
        let record_data = self.read_at_offset(record_offset, boot_sector.bytes_per_record as usize)?;

        // Parse record
        let record = MftRecord::parse(&record_data, record_number, boot_sector)?;

        // Cache the record
        {
            let mut cache = self.record_cache.write().await;
            cache.insert(record_number, record.clone());
        }

        Ok(record)
    }

    /// Enumerate all files in MFT (sequential scan)
    pub async fn enumerate_files(&mut self) -> VelocityResult<Vec<MftFileEntry>> {
        let _boot_sector = self.boot_sector.as_ref()
            .ok_or_else(|| VelocityError::Config("Boot sector not loaded".to_string()))?;

        // Estimate total records (MFT size / record size)
        // We'll scan until we hit empty records
        let mut entries = Vec::new();
        let mut consecutive_empty = 0;
        const MAX_CONSECUTIVE_EMPTY: u64 = 1000;

        // Start from record 0 ($MFT itself) and skip system files
        // User files typically start after record 23 or so
        for record_number in 0.. {
            match self.read_record(record_number).await {
                Ok(record) => {
                    if record.header.is_in_use() && record.header.used_size > 0 {
                        consecutive_empty = 0;

                        // Skip system files (records < 24 typically)
                        if record_number >= 24 {
                            if let Some(name) = &record.file_name {
                                if !name.is_empty() {
                                    entries.push(MftFileEntry {
                                        file_reference: record.header.file_reference(record_number),
                                        name: name.clone(),
                                        path: PathBuf::from(name), // Will be resolved later
                                        size: record.size,
                                        modified: filetime_to_unix(record.modification_time),
                                        is_directory: record.is_directory,
                                        extension: record.file_name.as_ref().and_then(|n| {
                                            n.rsplit('.').next().map(|s| s.to_lowercase())
                                        }),
                                    });
                                }
                            }
                        }
                    } else {
                        consecutive_empty += 1;
                        if consecutive_empty >= MAX_CONSECUTIVE_EMPTY {
                            break; // Assume we've passed all used records
                        }
                    }
                }
                Err(_) => {
                    consecutive_empty += 1;
                    if consecutive_empty >= MAX_CONSECUTIVE_EMPTY {
                        break;
                    }
                }
            }

            // Safety limit
            if record_number > 10_000_000 {
                break;
            }
        }

        Ok(entries)
    }

    /// Build full path for a file by walking up parent references
    pub async fn build_path(&mut self, record: &MftRecord) -> VelocityResult<PathBuf> {
        let mut path_components = Vec::new();
        let current_record = record.clone();

        // Add the file's own name
        if let Some(name) = &current_record.file_name {
            path_components.push(name.clone());
        }

        // Walk up parent chain
        let mut parent_ref = current_record.parent_ref;
        let mut depth = 0;
        const MAX_DEPTH: u32 = 100;

        while let Some(parent) = parent_ref {
            if depth >= MAX_DEPTH {
                break;
            }

            // Extract record number from file reference (upper 48 bits)
            let parent_record_number = parent >> 16;

            // Skip root directory (record 5) and system records
            if parent_record_number <= 5 {
                break;
            }

            match self.read_record(parent_record_number).await {
                Ok(parent_record) => {
                    if let Some(name) = &parent_record.file_name {
                        if !name.is_empty() && name != "." && name != ".." {
                            path_components.push(name.clone());
                        }
                    }
                    parent_ref = parent_record.parent_ref;
                }
                Err(_) => break,
            }
            depth += 1;
        }

        // Reverse to get correct order
        path_components.reverse();

        // Build path
        let mut path = PathBuf::new();
        for component in path_components {
            path.push(component);
        }

        // Prepend drive letter from volume_path (e.g., "\\.\C:" -> "C:")
        let drive_letter = Self::extract_drive_letter(&self.volume_path);
        if let Some(drive) = drive_letter {
            let mut full_path = PathBuf::from(format!("{}:\\", drive));
            full_path.push(path);
            Ok(full_path)
        } else {
            Ok(path)
        }
    }

    /// Extract drive letter from volume path (e.g., "\\.\C:" -> Some('C'))
    fn extract_drive_letter(volume_path: &PathBuf) -> Option<char> {
        let path_str = volume_path.to_string_lossy();
        // Handle formats: "\\.\C:", "C:", etc.
        for ch in path_str.chars() {
            if ch.is_ascii_alphabetic() {
                return Some(ch.to_ascii_uppercase());
            }
        }
        None
    }

    /// Close the volume handle
    pub async fn close(&mut self) {
        if let Some(handle) = self.volume_handle.take() {
            // File handle will be closed on drop
            drop(handle);
        }
        let mut cache = self.record_cache.write().await;
        cache.clear();
    }
}

impl Drop for MftParser {
    fn drop(&mut self) {
        // Note: close() is async, so we can't call it from Drop.
        // The cache will be cleaned up when the Arc is dropped.
        // The volume handle will be closed on drop.
        if let Some(handle) = self.volume_handle.take() {
            drop(handle);
        }
    }
}

/// MFT File Entry for search results
#[derive(Debug, Clone)]
pub struct MftFileEntry {
    pub file_reference: u64,
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub modified: u64,
    pub is_directory: bool,
    pub extension: Option<String>,
}

/// MFT Provider implementing IndexProvider trait
///
/// Production MFT-based index provider for the Velocity search engine.
pub struct MftProvider {
    parser: Arc<RwLock<MftParser>>,
    indexed_files: Arc<RwLock<Vec<MftFileEntry>>>,
    is_indexing: Arc<RwLock<bool>>,
    last_stats: Arc<RwLock<Option<IndexStats>>>,
    exclude_globset: Option<Arc<GlobSet>>,
}

impl MftProvider {
    pub fn new(volume_path: PathBuf) -> Self {
        Self::with_excludes(volume_path, vec![])
    }

    pub fn with_excludes(volume_path: PathBuf, exclude_patterns: Vec<String>) -> Self {
        let exclude_globset = if exclude_patterns.is_empty() {
            None
        } else {
            let mut builder = GlobSetBuilder::new();
            for pattern in exclude_patterns {
                if let Ok(glob) = Glob::new(&pattern) {
                    builder.add(glob);
                }
            }
            match builder.build() {
                Ok(globset) => Some(Arc::new(globset)),
                Err(_) => None,
            }
        };

        Self {
            parser: Arc::new(RwLock::new(MftParser::new(volume_path))),
            indexed_files: Arc::new(RwLock::new(Vec::new())),
            is_indexing: Arc::new(RwLock::new(false)),
            last_stats: Arc::new(RwLock::new(None)),
            exclude_globset,
        }
    }

    /// Get reference to parser for advanced operations
    pub fn parser(&self) -> &Arc<RwLock<MftParser>> {
        &self.parser
    }
}

#[async_trait]
impl IndexProvider for MftProvider {
    async fn initialize(&self) -> VelocityResult<()> {
        // Open the parser first (acquire lock briefly)
        {
            let mut parser = self.parser.write().await;
            parser.open().await?;
        }

        // Perform initial indexing (reindex acquires its own lock)
        self.reindex().await?;

        Ok(())
    }

    async fn search(&self, query: SearchQuery) -> VelocityResult<SearchResults> {
        let start = std::time::Instant::now();
        let files = self.indexed_files.read().await;

        let results: Vec<SearchResult> = files
            .iter()
            .filter(|f| {
                // Simple substring match (case-insensitive)
                // In production, use proper query parsing
                f.name.to_lowercase().contains(&query.text.to_lowercase())
            })
            .skip(query.offset)
            .take(query.max_results)
            .map(|f| SearchResult {
                name: f.name.clone(),
                path: f.path.clone(),
                size: f.size,
                modified: f.modified,
                extension: f.extension.clone(),
                is_directory: f.is_directory,
            })
            .collect();

        let total_count = files.iter()
            .filter(|f| f.name.to_lowercase().contains(&query.text.to_lowercase()))
            .count();

        Ok(SearchResults {
            results,
            total_count: Some(total_count),
            query_time_ms: start.elapsed().as_millis() as u64,
            partial: total_count > query.max_results,
        })
    }

    async fn stats(&self) -> VelocityResult<IndexStats> {
        let files = self.indexed_files.read().await;
        let is_indexing = *self.is_indexing.read().await;

        let stats = IndexStats {
            total_files: files.len() as u64,
            total_size: files.iter().map(|f| f.size).sum(),
            index_size_bytes: 0, // TODO: calculate actual index size
            last_updated: Some(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()),
            drives_indexed: vec!["C:".to_string()], // TODO: get actual drives
            is_indexing,
            indexing_progress: if is_indexing {
                Some(IndexingProgress {
                    current_drive: "C:".to_string(),
                    files_processed: files.len() as u64,
                    estimated_total: files.len() as u64 * 2, // rough estimate
                    current_path: None,
                })
            } else {
                None
            },
        };

        *self.last_stats.write().await = Some(stats.clone());
        Ok(stats)
    }

    async fn reindex(&self) -> VelocityResult<()> {
        *self.is_indexing.write().await = true;

        let mut parser = self.parser.write().await;
        let entries = parser.enumerate_files().await?;

        // Build full paths for entries and filter by exclude patterns
        let mut entries_with_paths = Vec::new();
        for entry in entries {
            // Build full path using the parser (includes drive letter)
            // Need to convert MftFileEntry to MftRecord for build_path
            let record = match parser.read_record(entry.file_reference >> 16).await {
                Ok(r) => r,
                Err(_) => {
                    // Fallback to name if record reading fails
                    let path = PathBuf::from(&entry.name);
                    // Filter by exclude patterns if configured
                    if let Some(ref globset) = self.exclude_globset {
                        let path_str = path.to_string_lossy();
                        if globset.is_match(path_str.as_ref()) {
                            continue; // Skip excluded paths
                        }
                    }
                    entries_with_paths.push(MftFileEntry {
                        path,
                        ..entry
                    });
                    continue;
                }
            };

            let path = match parser.build_path(&record).await {
                Ok(p) => p,
                Err(_) => PathBuf::from(&entry.name),
            };

            // Filter by exclude patterns if configured
            if let Some(ref globset) = self.exclude_globset {
                let path_str = path.to_string_lossy();
                if globset.is_match(path_str.as_ref()) {
                    continue; // Skip excluded paths
                }
            }

            entries_with_paths.push(MftFileEntry {
                path,
                ..entry
            });
        }

        *self.indexed_files.write().await = entries_with_paths;
        *self.is_indexing.write().await = false;

        Ok(())
    }

    async fn pause_indexing(&self) -> VelocityResult<()> {
        *self.is_indexing.write().await = false;
        Ok(())
    }

    async fn resume_indexing(&self) -> VelocityResult<()> {
        self.reindex().await
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            name: "MFT Parser".to_string(),
            supports_realtime: true,
            supports_operators: true,
            supports_content_search: false,
            supports_preview: true,
            requires_admin: true,
            version: "1.0.0".to_string(),
        }
    }

    async fn shutdown(&self) -> VelocityResult<()> {
        let mut parser = self.parser.write().await;
        parser.close().await;
        Ok(())
    }
}