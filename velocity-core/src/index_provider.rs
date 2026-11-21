use crate::VelocityResult;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    pub max_results: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub name: String,
    pub path: PathBuf,
    pub size: Option<u64>,
    pub modified: Option<u64>, // Unix timestamp
    pub extension: Option<String>,
    pub is_directory: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    pub results: Vec<SearchResult>,
    /// Exact matching count when available. None means the provider could not determine it.
    pub total_count: Option<usize>,
    pub query_time_ms: u64,
    pub partial: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    pub total_files: u64,
    pub total_size: u64,
    pub index_size_bytes: u64,
    pub last_updated: Option<u64>,
    pub drives_indexed: Vec<String>,
    pub is_indexing: bool,
    pub indexing_progress: Option<IndexingProgress>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexingProgress {
    pub current_drive: String,
    pub files_processed: u64,
    pub estimated_total: u64,
    pub current_path: Option<String>,
}

#[async_trait::async_trait]
pub trait IndexProvider: Send + Sync {
    /// Initialize the provider (connect to Everything, open MFT, etc.)
    async fn initialize(&self) -> VelocityResult<()>;

    /// Search for files matching the query
    async fn search(&self, query: SearchQuery) -> VelocityResult<SearchResults>;

    /// Get index statistics
    async fn stats(&self) -> VelocityResult<IndexStats>;

    /// Start or trigger a full re-index
    async fn reindex(&self) -> VelocityResult<()>;

    /// Pause indexing
    async fn pause_indexing(&self) -> VelocityResult<()>;

    /// Resume indexing
    async fn resume_indexing(&self) -> VelocityResult<()>;

    /// Get provider capabilities
    fn capabilities(&self) -> ProviderCapabilities;

    /// Shutdown the provider
    async fn shutdown(&self) -> VelocityResult<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub name: String,
    pub supports_realtime: bool,
    pub supports_operators: bool,
    pub supports_content_search: bool,
    pub supports_preview: bool,
    pub requires_admin: bool,
    pub version: String,
}

// Re-export async_trait for consumers
pub use async_trait::async_trait;
