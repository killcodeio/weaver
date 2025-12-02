pub mod v2;

use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

use crate::core::binary::{BinaryInfo, OperatingSystem};
use crate::models::request::MergeMode;

/// Main entry point for binary merging
/// 
/// This function:
/// 1. Detects the architecture and OS of both binaries
/// 2. Validates they are compatible
/// 3. Routes to the unified V2 merger
pub async fn merge_binaries(
    base_data: &[u8],
    overload_data: &[u8],
    mode: MergeMode,
    sync: bool,
    temp_dir: &str,
    task_id: &str,
    redis_url: &str,
) -> Result<String> {
    // Comprehensive binary detection
    let base_info = BinaryInfo::detect(base_data);
    let overload_info = BinaryInfo::detect(overload_data);
    
    log::info!("🔍 Detected binaries:");
    log::info!("  Base: {}", base_info.description());
    log::info!("  Overload: {}", overload_info.description());
    
    // Validate compatibility
    if !base_info.is_compatible_with(&overload_info) {
        anyhow::bail!(
            "❌ Binary mismatch! Base is {} but overload is {}. Both binaries must have the same architecture and OS.",
            base_info.description(),
            overload_info.description()
        );
    }
    
    if !base_info.is_supported() {
        anyhow::bail!(
            "❌ Unsupported binary: {}. Supported: x86/x86-64/ARM/ARM64 on Linux/Windows/macOS",
            base_info.description()
        );
    }
    
    log::info!("✅ Binary validation passed: {}", base_info.description());
    
    // Create temp directory
    fs::create_dir_all(temp_dir)?;
    let work_dir = TempDir::new_in(temp_dir)?;
    let work_path = work_dir.path();
    
    log::info!("Working directory: {}", work_path.display());
    
    // Handle MergeMode by swapping binaries if necessary
    // If mode is Before, we treat overload as the "base" (primary) in some contexts,
    // but for loader-stub, it runs both. 
    // However, to respect the "Before" semantics (Overload runs "before" Base?), 
    // we might want to swap them if the stub executes them in order.
    // For now, we'll pass them as is, but log the mode.
    log::info!("Merge mode: {:?} (Using unified V2 loader-stub)", mode);

    // Use V2 merger for all platforms
    // Default settings for basic merge: grace_period=0, network_failure_kill_count=0
    let merged_path_str = v2::merge_v2(
        base_data,
        overload_data,
        work_path,
        &base_info,
        task_id,
        redis_url,
        0, // grace_period
        sync, // sync_mode
        0, // network_failure_kill_count
    ).await?;
    
    let merged_path = PathBuf::from(merged_path_str);

    // Copy to permanent location with UUID
    let final_path = PathBuf::from(temp_dir)
        .join(format!("merged_{}.bin", uuid::Uuid::new_v4()));
    
    fs::copy(&merged_path, &final_path)?;
    
    log::info!("✅ Final merged binary: {}", final_path.display());
    
    Ok(final_path.to_string_lossy().to_string())
}

/// Stop-on-exit merge entry point
/// Now uses the V2 implementation with default settings
pub async fn merge_stop_on_exit(
    base_data: &[u8],
    overload_data: &[u8],
    work_path: &std::path::Path,
    base_info: &BinaryInfo,
    task_id: &str,
    redis_url: &str,
) -> Result<String> {
    // Use V2 with defaults: grace_period=0, sync_mode=false, network_failure_kill_count=0
    v2::merge_v2(
        base_data,
        overload_data,
        work_path,
        base_info,
        task_id,
        redis_url,
        0,
        false,
        0
    ).await
}

/// V2 merge entry point with advanced health monitoring
pub async fn merge_v2_stop_on_exit(
    base_data: &[u8],
    overload_data: &[u8],
    work_path: &std::path::Path,
    base_info: &BinaryInfo,
    task_id: &str,
    redis_url: &str,
    grace_period: u32,
    sync_mode: bool,
    network_failure_kill_count: u32,
) -> Result<String> {
    v2::merge_v2(
        base_data,
        overload_data,
        work_path,
        base_info,
        task_id,
        redis_url,
        grace_period,
        sync_mode,
        network_failure_kill_count
    ).await
}
