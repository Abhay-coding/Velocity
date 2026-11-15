//! MFT Parser Test Binary
//!
//! Tests the MFT parser against the C: drive (read-only).
//! Requires administrator privileges to access raw volume.

use std::path::PathBuf;
use velocity_mft::{MftParser, MftProvider};
use velocity_core::{index_provider::*, VelocityResult};

#[tokio::main]
async fn main() -> VelocityResult<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    println!("=== Velocity MFT Parser Test ===");
    println!("Testing against C: drive (read-only)");
    println!("Note: This requires administrator privileges\n");

    // Test 1: Test MftParser directly
    println!("--- Test 1: MftParser Direct ---");
    test_mft_parser().await?;

    // Test 2: Test MftProvider (IndexProvider trait)
    println!("\n--- Test 2: MftProvider (IndexProvider) ---");
    test_mft_provider().await?;

    println!("\n=== All Tests Completed ===");
    Ok(())
}

async fn test_mft_parser() -> VelocityResult<()> {
    let volume_path = PathBuf::from(r"\\.\C:");
    println!("Opening volume: {}", volume_path.display());

    let mut parser = MftParser::new(volume_path.clone());

    match parser.open().await {
        Ok(_) => {
            println!("✓ Volume opened successfully");

            // Get boot sector info
            if let Some(bs) = parser.boot_sector() {
                println!("  Bytes per sector: {}", bs.bytes_per_sector);
                println!("  Sectors per cluster: {}", bs.sectors_per_cluster);
                println!("  Bytes per cluster: {}", bs.bytes_per_cluster);
                println!("  Total sectors: {}", bs.total_sectors);
                println!("  MFT LCN: {}", bs.mft_lcn);
                println!("  Bytes per record: {}", bs.bytes_per_record);
                println!("  Volume serial: 0x{:X}", bs.volume_serial);
                println!("  OEM ID: {}", bs.oem_id);
            }

            // Test reading a few records
            println!("\nReading first 30 MFT records...");
            for i in 0..30 {
                match parser.read_record(i).await {
                    Ok(record) => {
                        if record.header.is_in_use() && record.header.used_size > 0 {
                            if let Some(name) = &record.file_name {
                                if !name.is_empty() && i >= 24 {
                                    println!("  Record {}: {} (size: {}, dir: {})", i, name, record.size, record.is_directory);
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Skip errors for now
                    }
                }
            }

            // Test building path for a record
            println!("\nTesting path building for record 25 (if valid)...");
            if let Ok(record) = parser.read_record(25).await {
                if record.header.is_in_use() {
                    match parser.build_path(&record).await {
                        Ok(path) => println!("  Path: {}", path.display()),
                        Err(e) => println!("  Path build error: {}", e),
                    }
                }
            }

            parser.close().await;
            println!("✓ Volume closed");
        }
        Err(e) => {
            println!("✗ Failed to open volume: {}", e);
            println!("  Note: This requires administrator privileges!");
            return Err(e);
        }
    }

    Ok(())
}

async fn test_mft_provider() -> VelocityResult<()> {
    let volume_path = PathBuf::from(r"\\.\C:");
    println!("Creating MftProvider for: {}", volume_path.display());

    let provider = MftProvider::new(volume_path);

    // Initialize (opens volume and indexes)
    println!("Initializing provider (indexing C: drive)...");
    match provider.initialize().await {
        Ok(_) => {
            println!("✓ Provider initialized and indexed");

            // Get stats
            let stats = provider.stats().await?;
            println!("\nIndex Statistics:");
            println!("  Total files: {}", stats.total_files);
            println!("  Total size: {} bytes", stats.total_size);
            println!("  Drives indexed: {:?}", stats.drives_indexed);
            println!("  Is indexing: {}", stats.is_indexing);

            // Test search
            println!("\nTesting search for 'test'...");
            let query = SearchQuery {
                text: "test".to_string(),
                max_results: 10,
                offset: 0,
            };

            let results = provider.search(query).await?;
            println!("  Found {} results (showing up to 10):", results.total_count);
            for result in &results.results {
                println!("    {} ({})", result.name, result.path.display());
            }

            // Test search for 'windows'
            println!("\nTesting search for 'windows'...");
            let query = SearchQuery {
                text: "windows".to_string(),
                max_results: 10,
                offset: 0,
            };

            let results = provider.search(query).await?;
            println!("  Found {} results (showing up to 10):", results.total_count);
            for result in &results.results {
                println!("    {} ({})", result.name, result.path.display());
            }

            // Test search for 'program'
            println!("\nTesting search for 'program'...");
            let query = SearchQuery {
                text: "program".to_string(),
                max_results: 10,
                offset: 0,
            };

            let results = provider.search(query).await?;
            println!("  Found {} results (showing up to 10):", results.total_count);
            for result in &results.results {
                println!("    {} ({})", result.name, result.path.display());
            }

            // Shutdown
            provider.shutdown().await?;
            println!("\n✓ Provider shutdown complete");
        }
        Err(e) => {
            println!("✗ Failed to initialize provider: {}", e);
            println!("  Note: This requires administrator privileges!");
            return Err(e);
        }
    }

    Ok(())
}