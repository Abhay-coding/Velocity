//! velocity-mft: NTFS MFT Parser
//!
//! Experimental NTFS Master File Table parser for learning and research.
//! The scanner assumes a contiguous MFT and is not suitable for reliable indexing.

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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use globset::{Glob, GlobSet, GlobSetBuilder};
use windows::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER;

/// Experimental NTFS MFT parser
///
/// Provides direct access to NTFS Master File Table for fast file enumeration
/// and searching. Uses Windows APIs for volume access.
pub struct MftParser {
    volume_path: PathBuf,
    boot_sector: Option<BootSectorInfo>,
    volume_handle: Option<File>,
    record_count: Option<u64>,
    /// Cache of parsed records for performance
    record_cache: Arc<RwLock<HashMap<u64, MftRecord>>>,
}

impl MftParser {
    pub fn new(volume_path: PathBuf) -> Self {
        Self {
            volume_path,
            boot_sector: None,
            volume_handle: None,
            record_count: None,
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
        let handle = self.open_volume_handle(&volume_str)?;
        let volume_data = Self::query_volume_data(&handle)?;
        let (mft_offset, record_count) = Self::mft_bounds(&boot_sector, &volume_data)?;

        // The raw reader can only follow a contiguous MFT. Confirm that its
        // first data run covers every record before using linear disk offsets.
        let first_record = Self::read_at_offset(&handle, mft_offset, boot_sector.bytes_per_record as usize)?;
        let first_record = MftRecord::parse(&first_record, 0, &boot_sector)?;
        let first_run = first_record.attributes.iter().find_map(|attribute| match attribute {
            MftAttribute::Data(data) if !data.resident && data.name.is_none() => data.runlist.first(),
            _ => None,
        });
        let contiguous_len = first_run
            .filter(|run| u64::try_from(run.lcn).ok() == Some(boot_sector.mft_lcn))
            .and_then(|run| run.length.checked_mul(u64::from(boot_sector.bytes_per_cluster)));
        if !contiguous_len.is_some_and(|len| len >= volume_data.MftValidDataLength as u64) {
            return Err(VelocityError::Config(
                "The experimental MFT reader requires a contiguous master file table".to_string(),
            ));
        }

        self.record_cache.write().await.clear();
        self.boot_sector = Some(boot_sector);
        self.volume_handle = Some(handle);
        self.record_count = Some(record_count);

        Ok(())
    }

    fn query_volume_data(handle: &File) -> VelocityResult<NTFS_VOLUME_DATA_BUFFER> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::{Ioctl::FSCTL_GET_NTFS_VOLUME_DATA, IO::DeviceIoControl};

        let mut data = NTFS_VOLUME_DATA_BUFFER::default();
        let size = std::mem::size_of_val(&data) as u32;
        let mut returned = 0;
        // File owns the open handle for this synchronous call. Windows writes
        // at most size bytes to the initialized SDK structure.
        unsafe {
            DeviceIoControl(
                HANDLE(handle.as_raw_handle()),
                FSCTL_GET_NTFS_VOLUME_DATA,
                None,
                0,
                Some((&mut data as *mut NTFS_VOLUME_DATA_BUFFER).cast()),
                size,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|error| VelocityError::Config(format!("Failed to query NTFS volume data: {error}")))?;
        if returned < size {
            return Err(VelocityError::Config("Incomplete NTFS volume data".to_string()));
        }
        Ok(data)
    }

    fn mft_bounds(boot: &BootSectorInfo, data: &NTFS_VOLUME_DATA_BUFFER) -> VelocityResult<(u64, u64)> {
        let invalid = || VelocityError::Config("Invalid master file table bounds".to_string());
        let length = u64::try_from(data.MftValidDataLength).map_err(|_| invalid())?;
        let record_size = u64::from(boot.bytes_per_record);
        if record_size == 0
            || data.BytesPerFileRecordSegment != boot.bytes_per_record
            || data.BytesPerCluster != boot.bytes_per_cluster
            || u64::try_from(data.MftStartLcn).ok() != Some(boot.mft_lcn)
            || length == 0
            || length % record_size != 0
        {
            return Err(invalid());
        }
        let offset = boot.mft_lcn.checked_mul(u64::from(boot.bytes_per_cluster)).ok_or_else(invalid)?;
        let volume_length = boot.total_sectors.checked_mul(u64::from(boot.bytes_per_sector)).ok_or_else(invalid)?;
        if offset.checked_add(length).is_none_or(|end| end > volume_length) {
            return Err(invalid());
        }
        Ok((offset, length / record_size))
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
    fn read_at_offset(mut handle: &File, offset: u64, size: usize) -> VelocityResult<Vec<u8>> {
        let mut buffer = vec![0u8; size];
        // Use Seek + ReadExact for portable offset reading
        handle.seek(SeekFrom::Start(offset))
            .map_err(|e| VelocityError::Config(format!("Failed to seek to offset {}: {}", offset, e)))?;
        handle.read_exact(&mut buffer)
            .map_err(|e| VelocityError::Config(format!("Failed to read at offset {}: {}", offset, e)))?;

        Ok(buffer)
    }

    fn read_record_bytes(&self, record_number: u64, boot: &BootSectorInfo) -> VelocityResult<Vec<u8>> {
        let handle = self.volume_handle.as_ref()
            .ok_or_else(|| VelocityError::Config("Volume not opened".to_string()))?;
        if self.record_count.is_none_or(|count| record_number >= count) {
            return Err(VelocityError::Config("Record lies beyond the master file table".to_string()));
        }
        // open() validates this entire range before making the volume available.
        let offset = boot.mft_lcn * u64::from(boot.bytes_per_cluster)
            + record_number * u64::from(boot.bytes_per_record);
        Self::read_at_offset(handle, offset, boot.bytes_per_record as usize)
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

        let record_data = self.read_record_bytes(record_number, boot_sector)?;

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
        let boot_sector = self.boot_sector.as_ref()
            .ok_or_else(|| VelocityError::Config("Boot sector not loaded".to_string()))?;
        let record_count = self.record_count
            .ok_or_else(|| VelocityError::Config("Volume not opened".to_string()))?;
        self.record_cache.write().await.clear();
        let mut entries = Vec::new();
        for record_number in 24..record_count {
            let bytes = self.read_record_bytes(record_number, boot_sector)?;
            let Ok(record) = MftRecord::parse(&bytes, record_number, boot_sector) else {
                continue;
            };
            let Some(name) = record.file_name.as_deref().filter(|name| !name.is_empty()) else {
                continue;
            };
            if !record.header.is_in_use() {
                continue;
            }
            entries.push(MftFileEntry {
                file_reference: record.header.file_reference(record_number),
                name: name.to_owned(),
                path: PathBuf::from(name),
                size: record.size,
                modified: filetime_to_unix(record.modification_time),
                is_directory: record.is_directory,
                extension: name.rsplit('.').next().map(|suffix| suffix.to_lowercase()),
            });
            self.record_cache.write().await.insert(record_number, record);
        }

        Ok(entries)
    }

    /// Build full path for a file by walking up parent references
    pub async fn build_path(&mut self, record: &MftRecord) -> VelocityResult<PathBuf> {
        let mut path_components = Vec::new();
        let current_record = record.clone();

        // Add the file's own name
        if let Some(name) = &current_record.file_name {
            if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
                return Err(VelocityError::Config("Invalid file name in MFT record".to_string()));
            }
            path_components.push(name.clone());
        }

        // Walk up parent chain
        let mut parent_ref = current_record.parent_ref;
        let mut depth = 0;
        let mut visited = std::collections::HashSet::new();
        let mut reached_root = false;
        const MAX_DEPTH: u32 = 100;

        while let Some(parent) = parent_ref {
            if depth >= MAX_DEPTH || !visited.insert(parent) {
                return Err(VelocityError::Config("Invalid parent chain".to_string()));
            }

            // NTFS file references contain a low 48-bit record number and high 16-bit sequence.
            let parent_record_number = parent & 0x0000_ffff_ffff_ffff;

            // Skip root directory (record 5) and system records
            if parent_record_number == 5 {
                reached_root = true;
                break;
            }
            if parent_record_number < 5 {
                return Err(VelocityError::Config("Invalid parent record".to_string()));
            }

            match self.read_record(parent_record_number).await {
                Ok(parent_record) => {
                    if parent_record.header.sequence_number != (parent >> 48) as u16 {
                        return Err(VelocityError::Config("Stale parent file reference".to_string()));
                    }
                    let name = parent_record.file_name.as_ref()
                        .filter(|n| !n.is_empty() && *n != "." && *n != ".." && !n.contains(['/', '\\']))
                        .ok_or_else(|| VelocityError::Config("Invalid parent name".to_string()))?;
                    if !parent_record.is_directory {
                        return Err(VelocityError::Config("Parent is not a directory".to_string()));
                    }
                    path_components.push(name.clone());
                    parent_ref = parent_record.parent_ref;
                }
                Err(e) => return Err(e),
            }
            depth += 1;
        }
        if !reached_root || path_components.is_empty() {
            return Err(VelocityError::Config("Cannot resolve complete path".to_string()));
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
            Err(VelocityError::Config("Volume path does not contain a drive letter".to_string()))
        }
    }

    /// Extract drive letter from volume path (e.g., "\\.\C:" -> Some('C'))
    fn extract_drive_letter(volume_path: &Path) -> Option<char> {
        let path_str = volume_path.to_string_lossy();
        let path = path_str.strip_prefix(r"\\.\").unwrap_or(&path_str);
        let bytes = path.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            Some((bytes[0] as char).to_ascii_uppercase())
        } else {
            None
        }
    }

    /// Close the volume handle
    pub async fn close(&mut self) {
        if let Some(handle) = self.volume_handle.take() {
            // File handle will be closed on drop
            drop(handle);
        }
        self.record_count = None;
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
/// Experimental MFT-based provider; the scanner does not handle fragmented MFTs.
pub struct MftProvider {
    parser: Arc<RwLock<MftParser>>,
    indexed_drive: String,
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

        let indexed_drive = MftParser::extract_drive_letter(&volume_path)
            .map(|drive| format!("{drive}:"))
            .unwrap_or_else(|| volume_path.to_string_lossy().into_owned());
        Self {
            parser: Arc::new(RwLock::new(MftParser::new(volume_path))),
            indexed_drive,
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
                size: Some(f.size),
                modified: Some(f.modified),
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
            drives_indexed: vec![self.indexed_drive.clone()],
            is_indexing,
            indexing_progress: if is_indexing {
                Some(IndexingProgress {
                    current_drive: self.indexed_drive.clone(),
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

        let result = async {
            let mut parser = self.parser.write().await;
            let entries = parser.enumerate_files().await?;
            let mut entries_with_paths = Vec::new();
            for entry in entries {
                let record = parser.read_record(entry.file_reference & 0x0000_ffff_ffff_ffff).await?;
                if record.header.sequence_number != (entry.file_reference >> 48) as u16 {
                    return Err(VelocityError::Config("Stale indexed file reference".to_string()));
                }
                let path = parser.build_path(&record).await?;
                if self.exclude_globset.as_ref().is_some_and(|g| g.is_match(&path)) {
                    continue;
                }
                entries_with_paths.push(MftFileEntry { path, ..entry });
            }
            *self.indexed_files.write().await = entries_with_paths;
            Ok(())
        }.await;
        *self.is_indexing.write().await = false;
        result
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn geometry() -> BootSectorInfo {
        BootSectorInfo {
            bytes_per_sector: 512,
            sectors_per_cluster: 8,
            bytes_per_cluster: 4096,
            total_sectors: 30_000_000,
            mft_lcn: 4,
            mft_mirror_lcn: 50,
            bytes_per_record: 1024,
            bytes_per_index_record: 4096,
            volume_serial: 0,
            oem_id: "NTFS".to_string(),
        }
    }

    #[test]
    fn bounds_use_the_mft_length_without_a_fixed_record_cap() {
        let boot = geometry();
        let mut data = NTFS_VOLUME_DATA_BUFFER {
            MftStartLcn: 4,
            MftValidDataLength: 10_000_002 * 1024,
            BytesPerCluster: 4096,
            BytesPerFileRecordSegment: 1024,
            ..Default::default()
        };
        assert_eq!(MftParser::mft_bounds(&boot, &data).unwrap(), (16_384, 10_000_002));

        data.MftValidDataLength += 1;
        assert!(MftParser::mft_bounds(&boot, &data).is_err());
        data.MftValidDataLength = 30_000_000 * 512;
        assert!(MftParser::mft_bounds(&boot, &data).is_err());
    }

    fn named_record() -> [u8; 1024] {
        let mut record = [0u8; 1024];
        record[..4].copy_from_slice(b"FILE");
        record[4..6].copy_from_slice(&48u16.to_le_bytes());
        record[6..8].copy_from_slice(&3u16.to_le_bytes());
        record[16..18].copy_from_slice(&1u16.to_le_bytes());
        record[20..22].copy_from_slice(&64u16.to_le_bytes());
        record[22..24].copy_from_slice(&1u16.to_le_bytes());
        record[24..28].copy_from_slice(&164u32.to_le_bytes());
        record[28..32].copy_from_slice(&1024u32.to_le_bytes());
        record[48..50].copy_from_slice(&[0xaa, 0xbb]);
        record[510..512].copy_from_slice(&[0xaa, 0xbb]);
        record[1022..1024].copy_from_slice(&[0xaa, 0xbb]);
        // Resident $FILE_NAME attribute containing the one-character name "x".
        record[64..68].copy_from_slice(&0x30u32.to_le_bytes());
        record[68..72].copy_from_slice(&96u32.to_le_bytes());
        record[80..84].copy_from_slice(&68u32.to_le_bytes());
        record[84..86].copy_from_slice(&24u16.to_le_bytes());
        record[152] = 1;
        record[153] = 1;
        record[154..156].copy_from_slice(&(b'x' as u16).to_le_bytes());
        record[160..164].copy_from_slice(&u32::MAX.to_le_bytes());
        record
    }

    #[tokio::test]
    async fn scan_past_unused_records_and_stop_at_the_mft_boundary() {
        let boot = geometry();
        let path = std::env::temp_dir().join(format!("velocity-mft-gap-{}.bin", std::process::id()));
        let mut file = File::options().read(true).write(true).create_new(true).open(&path).unwrap();
        let record_number = 1025u64;
        let start = boot.mft_lcn * u64::from(boot.bytes_per_cluster);
        file.seek(SeekFrom::Start(start + record_number * 1024)).unwrap();
        file.write_all(&named_record()).unwrap();
        // A valid-looking record past the end must not be mistaken for MFT data.
        file.write_all(&named_record()).unwrap();

        let mut parser = MftParser::new(PathBuf::from(r"\\.\D:"));
        parser.boot_sector = Some(boot);
        parser.volume_handle = Some(file);
        parser.record_count = Some(record_number + 1);
        let entries = parser.enumerate_files().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "x");
        assert_eq!(entries[0].file_reference, (1u64 << 48) | record_number);
        assert!(parser.read_record(record_number + 1).await.is_err());
        drop(parser);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn stats_show_the_configured_drive_even_while_indexing() {
        let provider = MftProvider::new(PathBuf::from(r"\\.\d:"));
        assert_eq!(provider.stats().await.unwrap().drives_indexed, ["D:"]);
        *provider.is_indexing.write().await = true;
        let stats = provider.stats().await.unwrap();
        assert_eq!(stats.indexing_progress.unwrap().current_drive, "D:");
    }
}
