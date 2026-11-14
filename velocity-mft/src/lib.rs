//! velocity-mft: NTFS MFT Parser Learning Crate
//!
//! This crate is a **learning project** for understanding NTFS Master File Table (MFT) parsing.
//! It is NOT the production indexer for v1 — the production backend uses Everything CLI adapter.
//!
//! Learning goals:
//! - Understand NTFS structures: $MFT, FILE_RECORD_HEADER, ATTRIBUTE_RECORD
//! - Parse resident vs non-resident attributes
//! - Handle runlists for non-resident $DATA
//! - Parse reparse points, hard links, alternate data streams
//! - USN Journal reading (FSCTL_READ_USN_JOURNAL)
//! - Safe Rust abstractions over unsafe Windows APIs

use velocity_core::{error::VelocityError, index_provider::*, VelocityResult};
use async_trait::async_trait;
use std::path::PathBuf;

/// NTFS MFT Parser (learning implementation)
///
/// This is a work-in-progress. The production v1 uses `EverythingCliAdapter`.
/// This crate exists for learning NTFS internals.
pub struct MftParser {
    volume_path: PathBuf,
}

impl MftParser {
    pub fn new(volume_path: PathBuf) -> Self {
        Self { volume_path }
    }

    /// Open volume for raw MFT access (requires admin)
    ///
    /// Uses CreateFileW with FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
    /// on \\.\C: (or other volume)
    pub async fn open(&self) -> VelocityResult<()> {
        // TODO: Implement using windows-rs
        // Key APIs:
        // - CreateFileW
        // - DeviceIoControl with FSCTL_GET_NTFS_VOLUME_DATA
        // - DeviceIoControl with FSCTL_READ_FILE_RECORD
        // - DeviceIoControl with FSCTL_ENUM_USN_DATA
        Err(VelocityError::NotImplemented("MftParser::open not yet implemented".to_string()))
    }

    /// Read MFT record by file reference number
    pub async fn read_record(&self, _file_ref: u64) -> VelocityResult<MftRecord> {
        Err(VelocityError::NotImplemented("MftParser::read_record not yet implemented".to_string()))
    }

    /// Enumerate all files in MFT
    pub async fn enumerate_files(&self) -> VelocityResult<Vec<MftFileEntry>> {
        Err(VelocityError::NotImplemented("MftParser::enumerate_files not yet implemented".to_string()))
    }
}

/// Parsed MFT record
#[derive(Debug, Clone)]
pub struct MftRecord {
    pub file_reference: u64,
    pub sequence_number: u16,
    pub flags: u16,
    pub attributes: Vec<MftAttribute>,
}

/// MFT attribute (resident or non-resident)
#[derive(Debug, Clone)]
pub enum MftAttribute {
    StandardInformation(StandardInformation),
    FileName(FileNameAttribute),
    Data(DataAttribute),
    // Add more: ObjectId, SecurityDescriptor, VolumeName, VolumeInformation, etc.
}

#[derive(Debug, Clone)]
pub struct StandardInformation {
    pub creation_time: u64,
    pub modification_time: u64,
    pub mft_modification_time: u64,
    pub access_time: u64,
    pub file_attributes: u32,
}

#[derive(Debug, Clone)]
pub struct FileNameAttribute {
    pub parent_file_reference: u64,
    pub creation_time: u64,
    pub modification_time: u64,
    pub mft_modification_time: u64,
    pub access_time: u64,
    pub allocated_size: u64,
    pub real_size: u64,
    pub file_attributes: u32,
    pub name_length: u8,
    pub namespace: u8,
    pub name: String,
}

#[derive(Debug, Clone)]
pub enum DataAttribute {
    Resident(Vec<u8>),
    NonResident {
        start_vcn: u64,
        last_vcn: u64,
        runlist: Vec<RunlistEntry>,
    },
}

#[derive(Debug, Clone)]
pub struct RunlistEntry {
    pub length: u64,
    pub lcn: i64, // Logical Cluster Number (can be -1 for sparse)
}

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
/// This is the future production backend once MFT parser is complete.
/// Currently a stub — learning implementation in progress.
pub struct MftProvider {
    parser: MftParser,
}

impl MftProvider {
    pub fn new(volume_path: PathBuf) -> Self {
        Self {
            parser: MftParser::new(volume_path),
        }
    }
}

#[async_trait]
impl IndexProvider for MftProvider {
    async fn initialize(&self) -> VelocityResult<()> {
        Err(VelocityError::NotImplemented("MftProvider not yet implemented".to_string()))
    }

    async fn search(&self, _query: SearchQuery) -> VelocityResult<SearchResults> {
        Err(VelocityError::NotImplemented("MftProvider::search not yet implemented".to_string()))
    }

    async fn stats(&self) -> VelocityResult<IndexStats> {
        Err(VelocityError::NotImplemented("MftProvider::stats not yet implemented".to_string()))
    }

    async fn reindex(&self) -> VelocityResult<()> {
        Err(VelocityError::NotImplemented("MftProvider::reindex not yet implemented".to_string()))
    }

    async fn pause_indexing(&self) -> VelocityResult<()> {
        Err(VelocityError::NotImplemented("MftProvider::pause_indexing not yet implemented".to_string()))
    }

    async fn resume_indexing(&self) -> VelocityResult<()> {
        Err(VelocityError::NotImplemented("MftProvider::resume_indexing not yet implemented".to_string()))
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            name: "MFT Parser (Learning)".to_string(),
            supports_realtime: true,
            supports_operators: false,
            supports_content_search: false,
            supports_preview: false,
            requires_admin: true,
            version: "0.1.0-learning".to_string(),
        }
    }

    async fn shutdown(&self) -> VelocityResult<()> {
        Ok(())
    }
}

// Learning resources for NTFS MFT parsing:
//
// 1. **NTFS Documentation**:
//    - Microsoft: "NTFS File System" (docs.microsoft.com)
//    - ReactOS NTFS driver (open source reference)
//    - NTFS-3G source code (userspace NTFS)
//
// 2. **Key Structures to Learn**:
//    - FILE_RECORD_HEADER (0x46494C45 "FILE" magic)
//    - ATTRIBUTE_RECORD_HEADER (resident vs non-resident)
//    - $STANDARD_INFORMATION (0x10)
//    - $FILE_NAME (0x30)
//    - $DATA (0x80) - resident vs non-resident
//    - $INDEX_ROOT (0x90), $INDEX_ALLOCATION (0xA0), $BITMAP (0xB0)
//    - $REPARSE_POINT (0xC0)
//
// 3. **Windows APIs** (via windows-rs):
//    - CreateFileW with \\.\C:
//    - DeviceIoControl with:
//      - FSCTL_GET_NTFS_VOLUME_DATA
//      - FSCTL_READ_FILE_RECORD
//      - FSCTL_ENUM_USN_DATA
//      - FSCTL_CREATE_USN_JOURNAL
//      - FSCTL_READ_USN_JOURNAL
//      - FSCTL_QUERY_USN_JOURNAL
//
// 4. **USN Journal**:
//    - USN_RECORD_V2/V3 structures
//    - Reason codes (FILE_CREATE, FILE_DELETE, DATA_EXTEND, etc.)
//    - Journal wrap handling (UsnJournalID change)
//
// 5. **Helpful Crates**:
//    - `windows-rs` - Microsoft's official Win32 bindings
//    - `raw_ntfs` - Low-level sector reading (reference only)
//    - `ntfs` - Unmaintained, incomplete (avoid for production)

// 6. **Practice Exercises**:
//    - [ ] Read volume $MFT and parse first 10 records
//    - [ ] Extract all $FILE_NAME attributes for a directory
//    - [ ] Handle non-resident $DATA with runlists
//    - [ ] Parse USN journal and replay changes
//    - [ ] Build a simple file walker from MFT
//
// Note: This crate is a learning project. The production v1 backend
// uses `EverythingCliAdapter` from `velocity-everything`.