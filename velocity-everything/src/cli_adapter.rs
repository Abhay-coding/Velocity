use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tracing::{debug, info, warn};

use velocity_core::{
    error::{VelocityError, VelocityResult},
    index_provider::{
        IndexProvider, IndexStats, IndexingProgress, ProviderCapabilities, SearchQuery,
        SearchResult, SearchResults,
    },
};

pub struct EverythingCliAdapter {
    es_path: PathBuf,
    max_results: usize,
    timeout: Duration,
}

impl EverythingCliAdapter {
    pub fn new(es_path: Option<PathBuf>) -> Self {
        let path = es_path.unwrap_or_else(|| {
            // Default locations to search for es.exe
            let candidates = vec![
                PathBuf::from("es.exe"),
                PathBuf::from(r"D:\Everything\es.exe"),
                PathBuf::from(r"C:\Program Files\Everything\es.exe"),
                PathBuf::from(r"C:\Program Files (x86)\Everything\es.exe"),
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|p| p.join("es.exe")))
                    .unwrap_or_else(|| PathBuf::from("es.exe")),
            ];
            candidates
                .into_iter()
                .find(|p| p.exists())
                .unwrap_or_else(|| PathBuf::from("es.exe"))
        });
        Self {
            es_path: path,
            max_results: 100,
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_max_results(mut self, max: usize) -> Self {
        self.max_results = max;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn build_search_args(&self, query: &SearchQuery) -> Vec<String> {
        let mut args = vec![
            "-n".to_string(),
            query.max_results.to_string(),
            "-s".to_string(), // sort by name
            query.text.clone(),
        ];
        if query.offset > 0 {
            args.extend(["-o".to_string(), query.offset.to_string()]);
        }
        args
    }

    fn parse_output(&self, output: &str) -> VelocityResult<Vec<SearchResult>> {
        let mut results = Vec::new();
        for line in output.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let path = PathBuf::from(line);
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(line)
                .to_string();

            let is_directory = std::fs::metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false);

            let metadata = std::fs::metadata(&path).ok();
            let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = metadata
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            let extension = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_lowercase());

            results.push(SearchResult {
                name,
                path,
                size,
                modified,
                extension,
                is_directory,
            });
        }
        Ok(results)
    }
}

#[async_trait::async_trait]
impl IndexProvider for EverythingCliAdapter {
    async fn initialize(&self) -> VelocityResult<()> {
        // Check if es.exe exists and is executable
        // We just verify the file exists and is executable, without running it
        // because es.exe -h returns exit code 1 even when successful
        if !self.es_path.exists() {
            return Err(VelocityError::EverythingNotFound(format!(
                "es.exe not found at: {}",
                self.es_path.display()
            )));
        }

        // Try a simple test search that should always work (search for nothing)
        // Use -n 0 to limit results to 0, which should always succeed
        let output = Command::new(&self.es_path)
            .args(["-n", "0", ""])
            .output()
            .await
            .map_err(|e| VelocityError::EverythingNotFound(format!(
                "Failed to execute {}: {}",
                self.es_path.display(),
                e
            )))?;

        // es.exe returns exit code 1 for help, but for searches it returns 0 on success
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Check if it's just the help output (which returns 1 but is actually success)
            if stderr.contains("Display this help") || stderr.contains("Search the database") {
                info!("Everything CLI adapter initialized (help output detected)");
                return Ok(());
            }
            return Err(VelocityError::EverythingNotFound(format!(
                "es.exe returned error: {}",
                stderr
            )));
        }

        info!("Everything CLI adapter initialized");
        Ok(())
    }

    async fn search(&self, query: SearchQuery) -> VelocityResult<SearchResults> {
        let start = Instant::now();
        let args = self.build_search_args(&query);

        debug!("Executing es.exe with args: {:?}", args);

        let output = tokio::time::timeout(self.timeout, async {
            Command::new(&self.es_path)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
        })
        .await
        .map_err(|_| VelocityError::IndexProvider("Search timeout".to_string()))?
        .map_err(|e| VelocityError::IndexProvider(format!("Failed to spawn es.exe: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("es.exe search failed: {}", stderr);
            return Err(VelocityError::IndexProvider(format!(
                "Search failed: {}",
                stderr
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let results = self.parse_output(&stdout)?;

        let total_count = results.len();
        let query_time_ms = start.elapsed().as_millis() as u64;

        Ok(SearchResults {
            results,
            total_count,
            query_time_ms,
            partial: false,
        })
    }

    async fn stats(&self) -> VelocityResult<IndexStats> {
        // Get total count by searching with empty query
        let query = SearchQuery {
            text: String::new(),
            max_results: 1,
            offset: 0,
        };

        let results = self.search(query).await?;

        Ok(IndexStats {
            total_files: results.total_count as u64,
            total_size: 0, // Not available from es.exe easily
            index_size_bytes: 0,
            last_updated: None,
            drives_indexed: vec![],
            is_indexing: false,
            indexing_progress: None,
        })
    }

    async fn reindex(&self) -> VelocityResult<()> {
        // Everything re-indexes automatically via USN journal
        // We can trigger a forced rebuild via IPC if needed
        info!("Reindex requested - Everything handles this automatically");
        Ok(())
    }

    async fn pause_indexing(&self) -> VelocityResult<()> {
        // Not directly supported via CLI
        warn!("Pause indexing not supported via Everything CLI");
        Ok(())
    }

    async fn resume_indexing(&self) -> VelocityResult<()> {
        // Not directly supported via CLI
        warn!("Resume indexing not supported via Everything CLI");
        Ok(())
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            name: "Everything CLI".to_string(),
            supports_realtime: true, // Via USN journal
            supports_operators: true, // ext:, path:, size:, etc.
            supports_content_search: false,
            supports_preview: false,
            requires_admin: false, // es.exe runs as user, Everything service runs as SYSTEM
            version: "1.0".to_string(),
        }
    }

    async fn shutdown(&self) -> VelocityResult<()> {
        Ok(())
    }
}