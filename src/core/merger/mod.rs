pub mod linux;
pub mod windows;
pub mod macos;

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
/// 3. Routes to the appropriate OS-specific merger
pub async fn merge_binaries(
    base_data: &[u8],
    overload_data: &[u8],
    mode: MergeMode,
    sync: bool,
    temp_dir: &str,
    task_id: &str,
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
    
    // Convert mode to string
    let mode_str = match mode {
        MergeMode::Before => "before",
        MergeMode::After => "after",
    };
    
    // Create temp directory
    fs::create_dir_all(temp_dir)?;
    let work_dir = TempDir::new_in(temp_dir)?;
    let work_path = work_dir.path();
    
    log::info!("Working directory: {}", work_path.display());
    
    // Route to OS-specific merger (original behavior for /merge endpoint)
    let merged_path = match base_info.os {
        OperatingSystem::Linux => {
            linux::basic::merge_linux_elf(base_data, overload_data, mode_str, sync, work_path, &base_info, task_id).await?
        }
        OperatingSystem::Windows => {
            windows::basic::merge_windows_pe(base_data, overload_data, mode_str, sync, work_path)?
        }
        OperatingSystem::MacOS => {
            macos::merge_macos_macho(base_data, overload_data, mode_str, sync, work_path)?
        }
        _ => {
            anyhow::bail!(
                "Unsupported OS: {}. Currently supported: Linux ✅, Windows ✅, macOS 🚧",
                base_info.os.name()
            )
        }
    };
    
    // Copy to permanent location with UUID
    let final_path = PathBuf::from(temp_dir)
        .join(format!("merged_{}.bin", uuid::Uuid::new_v4()));
    
    fs::copy(&merged_path, &final_path)?;
    
    log::info!("✅ Final merged binary: {}", final_path.display());
    
    Ok(final_path.to_string_lossy().to_string())
}

/// Stop-on-exit merge entry point
pub async fn merge_stop_on_exit(
    base_data: &[u8],
    overload_data: &[u8],
    work_path: &std::path::Path,
    base_info: &BinaryInfo,
    task_id: &str,
) -> Result<String> {
    // Route based on OS
    match base_info.os {
        OperatingSystem::Linux => {
            linux::stop_on_exit::merge_linux_elf_stop_on_exit(
                base_data,
                overload_data,
                work_path,
                base_info,
                task_id,
            ).await
        }
        OperatingSystem::Windows => {
            windows::stop_on_exit::merge_windows_pe_stop_on_exit(
                base_data,
                overload_data,
                work_path,
            )
        }
        _ => {
            anyhow::bail!(
                "Stop-on-exit mode only supported for Linux and Windows. Detected: {}",
                base_info.description()
            )
        }
    }
}

/// V2 merge entry point with advanced health monitoring
pub async fn merge_v2_stop_on_exit(
    base_data: &[u8],
    overload_data: &[u8],
    work_path: &std::path::Path,
    base_info: &BinaryInfo,
    task_id: &str,
    grace_period: u32,
    sync_mode: bool,
    network_failure_kill_count: u32,
) -> Result<String> {
    // Route based on OS
    match base_info.os {
        OperatingSystem::Linux => {
            linux::v2_stop_on_exit::merge_linux_elf_v2_stop_on_exit(
                base_data,
                overload_data,
                work_path,
                base_info,
                task_id,
                grace_period,
                sync_mode,
                network_failure_kill_count,
            ).await
        }
        OperatingSystem::Windows => {
            windows::v2_stop_on_exit::merge_windows_pe_v2_stop_on_exit(
                base_data,
                overload_data,
                work_path,
                grace_period,
                sync_mode,
                network_failure_kill_count,
            )
        }
        _ => {
            anyhow::bail!(
                "V2 merge only supports Linux and Windows. Detected: {}",
                base_info.description()
            )
        }
    }
}
