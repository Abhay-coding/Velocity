use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tracing::{debug, info, warn};

use velocity_core::{
    error::{VelocityError, VelocityResult},
    index_provider::{
        IndexProvider, IndexStats, ProviderCapabilities, SearchQuery,
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

    async fn parse_output(&self, output: &str) -> VelocityResult<Vec<SearchResult>> {
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

            // Use tokio::fs::metadata for non-blocking I/O
            let metadata = tokio::fs::metadata(&path).await.ok();
            let is_directory = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
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

        info!("Executing es.exe with args: {:?}", args);
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

        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        info!("es.exe exit code: {}, stdout len: {}, stderr len: {}", exit_code, stdout.len(), stderr.len());
        debug!("es.exe stdout: {}", stdout);
        debug!("es.exe stderr: {}", stderr);

        if !output.status.success() {
            warn!("es.exe search failed with exit code {}: {}", exit_code, stderr);
            return Err(VelocityError::IndexProvider(format!(
                "Search failed (exit code {}): {}",
                exit_code, stderr
            )));
        }

        let results = self.parse_output(&stdout).await?;
        let results_count = results.len();

        // For total_count, we need to get the actual total from Everything
        // We can do a separate count query, or use the results length if it's less than max_results
        // If results.len() < max_results, then total_count = results.len()
        // Otherwise, we don't know the exact total without a separate count query
        let total_count = if results_count < query.max_results {
            results_count
        } else {
            // Results are capped at max_results, we can't know exact total without another query
            // Return the count we got as a minimum
            results_count
        };

        let query_time_ms = start.elapsed().as_millis() as u64;

        info!("Search completed: {} results (returned), {}ms", results_count, query_time_ms);

        Ok(SearchResults {
            results,
            total_count,
            query_time_ms,
            partial: results_count >= query.max_results,
        })
    }

    async fn stats(&self) -> VelocityResult<IndexStats> {
        // Try to get actual total count using es.exe count flag
        // First try: use -c flag for count (es.exe 1.1.0.18+)
        let count_output = Command::new(&self.es_path)
            .args(["-c", ""])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    String::from_utf8(output.stdout).ok()
                } else {
                    None
                }
            })
            .and_then(|stdout| stdout.trim().parse::<u64>().ok());

        let total_files = if let Some(count) = count_output {
            count
        } else {
            // Fallback: use search with reasonable max_results
            let max_results = 10000;
            let query = SearchQuery {
                text: String::new(),
                max_results,
                offset: 0,
            };
            let results = self.search(query).await?;
            let total_count = results.total_count;
            if total_count < max_results {
                total_count as u64
            } else {
                total_count as u64 // At least this many
            }
        };

        Ok(IndexStats {
            total_files,
            total_size: 0, // Not available from es.exe easily
            index_size_bytes: 0,
            last_updated: None,
            drives_indexed: vec![],
            is_indexing: false,
            indexing_progress: None,
        })
    }

    async fn reindex(&self) -> VelocityResult<()> {
        info!("Reindex requested - Everything handles this automatically");
        Ok(())
    }

    async fn pause_indexing(&self) -> VelocityResult<()> {
        warn!("Pause indexing not supported via Everything CLI");
        Ok(())
    }

    async fn resume_indexing(&self) -> VelocityResult<()> {
        warn!("Resume indexing not supported via Everything CLI");
        Ok(())
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            name: "Everything CLI".to_string(),
            supports_realtime: true,
            supports_operators: true,
            supports_content_search: false,
            supports_preview: false,
            requires_admin: false,
            version: "1.0".to_string(),
        }
    }

    async fn shutdown(&self) -> VelocityResult<()> {
        Ok(())
    }
}