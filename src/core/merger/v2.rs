use anyhow::{Result, Context};
use std::path::Path;
use std::fs;
use std::mem;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use crate::core::binary::{BinaryInfo, OperatingSystem, Architecture};
use crate::core::progress::{ProgressTracker, ProgressStep};

// Embed the pre-compiled stubs for each OS/Architecture combination
// Note: These paths point to the /stubs directory in the Docker container // if run cargo check or build, outside the docker compose, it'll give errs as these files won't be found and is needed on compile time to be embedded in the binary

// Linux stubs
const LINUX_X86_64_STUB: &[u8] = include_bytes!("/stubs/linux-x86_64-stub");
const LINUX_X86_STUB: &[u8] = include_bytes!("/stubs/linux-x86-stub");
const LINUX_AARCH64_STUB: &[u8] = include_bytes!("/stubs/linux-aarch64-stub");

// Windows stubs
const WINDOWS_X86_64_STUB: &[u8] = include_bytes!("/stubs/windows-x86_64-stub.exe");
const WINDOWS_X86_STUB: &[u8] = include_bytes!("/stubs/windows-x86-stub.exe");
const WINDOWS_AARCH64_STUB: &[u8] = include_bytes!("/stubs/windows-aarch64-stub.exe");

// macOS stubs
const MACOS_X86_64_STUB: &[u8] = include_bytes!("/stubs/macos-x86_64-stub");
const MACOS_AARCH64_STUB: &[u8] = include_bytes!("/stubs/macos-aarch64-stub");

#[repr(C)]
struct ConfigFooter {
    magic: [u8; 8],
    base_offset: u64,
    base_size: u64,
    overload_offset: u64,
    overload_size: u64,
    grace_period: u32,
    sync_mode: u8,
    network_failure_kill_count: u32,
}

pub async fn merge_v2(
    base_data: &[u8],
    overload_data: &[u8],
    work_path: &Path,
    base_info: &BinaryInfo,
    task_id: &str,
    redis_url: &str,
    grace_period: u32,
    sync_mode: bool,
    network_failure_kill_count: u32,
) -> Result<String> {
    log::info!("🧬 V2 Merging binaries with pre-compiled Rust stub...");

    // Initialize progress tracker
    let progress_tracker = if !task_id.is_empty() {
        match ProgressTracker::new(redis_url, task_id.to_string()) {
            Ok(tracker) => Some(tracker),
            Err(e) => {
                log::warn!("Failed to create progress tracker: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Report: Detecting platforms
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::DetectingPlatforms).await;
    }

    // Select stub based on OS and Architecture
    let stub_bytes = match (&base_info.os, &base_info.arch) {
        // Linux
        (OperatingSystem::Linux, Architecture::X86_64) => LINUX_X86_64_STUB,
        (OperatingSystem::Linux, Architecture::X86) => LINUX_X86_STUB,
        (OperatingSystem::Linux, Architecture::AArch64) => LINUX_AARCH64_STUB,
        (OperatingSystem::Linux, arch) => {
            anyhow::bail!("Unsupported Linux architecture: {:?}. Supported: x86_64, x86, aarch64", arch)
        }
        
        // Windows
        (OperatingSystem::Windows, Architecture::X86_64) => WINDOWS_X86_64_STUB,
        (OperatingSystem::Windows, Architecture::X86) => WINDOWS_X86_STUB,
        (OperatingSystem::Windows, Architecture::AArch64) => WINDOWS_AARCH64_STUB,
        (OperatingSystem::Windows, arch) => {
            anyhow::bail!("Unsupported Windows architecture: {:?}. Supported: x86_64, x86, aarch64", arch)
        }
        
        // macOS
        (OperatingSystem::MacOS, Architecture::X86_64) => MACOS_X86_64_STUB,
        (OperatingSystem::MacOS, Architecture::AArch64) => MACOS_AARCH64_STUB,
        (OperatingSystem::MacOS, arch) => {
            anyhow::bail!("Unsupported macOS architecture: {:?}. Supported: x86_64, aarch64", arch)
        }
        
        // Other OS
        (os, _) => anyhow::bail!("Unsupported OS: {:?}", os),
    };

    log::info!("📦 Selected stub for {:?}/{:?} ({} bytes)", base_info.os, base_info.arch, stub_bytes.len());

    // Check if stub is valid (not a dummy/empty stub from dev build)
    if stub_bytes.is_empty() {
        anyhow::bail!(
            "Stub for {:?}/{:?} is empty. This platform may not be supported in the dev build. \
            Use a production build for full platform support.",
            base_info.os, base_info.arch
        );
    }

    let output_filename = if base_info.os == OperatingSystem::Windows { "merged.exe" } else { "merged" };
    let output_path = work_path.join(output_filename);

    // Calculate offsets
    let stub_len = stub_bytes.len() as u64;
    let base_len = base_data.len() as u64;
    let overload_len = overload_data.len() as u64;

    let base_offset = stub_len;
    let overload_offset = base_offset + base_len;

    // Create footer
    let footer = ConfigFooter {
        magic: *b"KILLCODE",
        base_offset,
        base_size: base_len,
        overload_offset,
        overload_size: overload_len,
        grace_period,
        sync_mode: if sync_mode { 1 } else { 0 },
        network_failure_kill_count,
    };

    // Serialize footer
    let footer_bytes = unsafe {
        std::slice::from_raw_parts(
            &footer as *const ConfigFooter as *const u8,
            mem::size_of::<ConfigFooter>()
        )
    };

    log::info!("📦 Constructing binary: Stub ({} bytes) + Base ({} bytes) + Overload ({} bytes) + Footer ({} bytes)", 
             stub_len, base_len, overload_len, footer_bytes.len());

    // Report: Compiling wrapper (Actually just assembling)
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::CompilingLoader).await;
    }

    // Write everything to output file
    let mut output_file = fs::File::create(&output_path)
        .context("Failed to create output file")?;
    
    output_file.write_all(stub_bytes).context("Failed to write stub")?;
    output_file.write_all(base_data).context("Failed to write base binary")?;
    output_file.write_all(overload_data).context("Failed to write overload binary")?;
    output_file.write_all(footer_bytes).context("Failed to write footer")?;

    // Make executable (skip for Windows if running on Linux, but doesn't hurt)
    if base_info.os != OperatingSystem::Windows {
        let mut perms = output_file.metadata()?.permissions();
        perms.set_mode(0o755);
        output_file.set_permissions(perms)?;
    }

    // Report: Finalizing
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::Finalizing).await;
    }

    Ok(output_path.to_string_lossy().into_owned())
}
