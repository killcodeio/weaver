use anyhow::{Result, Context};
use std::process::Command;
use std::path::Path;
use std::fs;
use std::io::Write;

use crate::core::binary::{BinaryInfo, CompilerConfig};
use crate::core::progress::{ProgressTracker, ProgressStep};

/// The C loader stub template for Linux ELF binaries
const LOADER_STUB_TEMPLATE: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <fcntl.h>
#include <errno.h>

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif

extern char _binary_first_start[];
extern char _binary_first_end[];
extern char _binary_second_start[];
extern char _binary_second_end[];

static int execute_binary(char* binary_data, size_t binary_size, const char* name, int wait_for_completion) {
    int fd = memfd_create(name, MFD_CLOEXEC);
    if (fd < 0) {
        perror("memfd_create failed");
        return -1;
    }
    
    if (write(fd, binary_data, binary_size) != (ssize_t)binary_size) {
        perror("Failed to write binary");
        close(fd);
        return -1;
    }
    
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork failed");
        close(fd);
        return -1;
    }
    
    if (pid == 0) {
        char fd_path[32];
        snprintf(fd_path, sizeof(fd_path), "/proc/self/fd/%d", fd);
        execl(fd_path, name, NULL);
        perror("execl failed");
        _exit(1);
    }
    
    if (wait_for_completion) {
        int status;
        waitpid(pid, &status, 0);
        close(fd);
        return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    }
    
    close(fd);
    return 0;
}

int main(int argc, char** argv) {
    size_t first_size = _binary_first_end - _binary_first_start;
    size_t second_size = _binary_second_end - _binary_second_start;
    
    int sync_mode = SYNC_MODE_PLACEHOLDER;
    
    if (execute_binary(_binary_first_start, first_size, "first", sync_mode) != 0) {
        fprintf(stderr, "Failed to execute first binary\n");
        return 1;
    }
    
    if (execute_binary(_binary_second_start, second_size, "second", 1) != 0) {
        fprintf(stderr, "Failed to execute second binary\n");
        return 1;
    }
    
    return 0;
}
"#;

/// Merge two Linux ELF binaries
pub async fn merge_linux_elf(
    base_data: &[u8],
    overload_data: &[u8],
    mode: &str,
    sync: bool,
    work_dir_path: &Path,
    base_info: &BinaryInfo,
    task_id: &str,
) -> Result<String> {
    log::info!("🐧 Merging Linux ELF binaries...");
    
    // Initialize progress tracker if task_id is provided
    let progress_tracker = if !task_id.is_empty() {
        log::info!("Initializing progress tracker for task: {}", task_id);
        match ProgressTracker::new("redis://redis:6379", task_id.to_string()) {
            Ok(tracker) => {
                log::info!("Progress tracker initialized successfully");
                Some(tracker)
            }
            Err(e) => {
                log::warn!("Failed to create progress tracker: {}", e);
                None
            }
        }
    } else {
        log::info!("No task_id provided, progress tracking disabled");
        None
    };
    
    // Report: Detecting platforms
    if let Some(ref tracker) = progress_tracker {
        log::info!("Reporting progress: Detecting platforms");
        match tracker.update(ProgressStep::DetectingPlatforms).await {
            Ok(_) => log::info!("Progress update sent successfully"),
            Err(e) => log::error!("Failed to send progress update: {}", e),
        }
    }
    
    // Get compiler configuration
    let compiler_config = CompilerConfig::for_binary(base_info);
    
    if !compiler_config.is_available() {
        anyhow::bail!(
            "❌ Required compiler not found: {}. Please install the appropriate cross-compiler.",
            compiler_config.gcc
        );
    }
    
    log::info!("✅ Using compiler: {}", compiler_config.gcc);
    
    // Report: Validating platforms
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::ValidatingPlatforms).await;
    }
    
    // Determine order based on mode
    let (first_data, second_data) = match mode {
        "before" => (overload_data, base_data),  // Overload runs first
        "after" => (base_data, overload_data),   // Base runs first
        _ => (overload_data, base_data),
    };
    
    log::info!("Merge mode: {}, Sync: {}", mode, sync);
    
    // Write binaries to temp files
    let first_path = work_dir_path.join("first");
    let second_path = work_dir_path.join("second");
    
    // Report: Writing binaries with dynamic progress
    let total_size = (first_data.len() + second_data.len()) as u64;
    let mut bytes_written = 0u64;
    
    // Write first binary in chunks with progress reporting
    let mut first_file = fs::File::create(&first_path)?;
    for chunk in first_data.chunks(8192) {
        first_file.write_all(chunk)?;
        bytes_written += chunk.len() as u64;
        
        if let Some(ref tracker) = progress_tracker {
            if bytes_written % (100 * 1024) == 0 || bytes_written == first_data.len() as u64 {
                let _ = tracker.report_io_progress(bytes_written, total_size, ProgressStep::WritingBinaries).await;
            }
        }
    }
    
    // Write second binary in chunks with progress reporting
    let mut second_file = fs::File::create(&second_path)?;
    for chunk in second_data.chunks(8192) {
        second_file.write_all(chunk)?;
        bytes_written += chunk.len() as u64;
        
        if let Some(ref tracker) = progress_tracker {
            if bytes_written % (100 * 1024) == 0 || bytes_written >= total_size {
                let _ = tracker.report_io_progress(bytes_written, total_size, ProgressStep::WritingBinaries).await;
            }
        }
    }
    
    log::info!("Wrote binaries: first={} bytes, second={} bytes", 
               first_data.len(), second_data.len());
    
    // Create loader stub with sync mode
    let loader_code = LOADER_STUB_TEMPLATE.replace(
        "SYNC_MODE_PLACEHOLDER",
        if sync { "1" } else { "0" }
    );
    
    let loader_path = work_dir_path.join("loader_stub.c");
    fs::write(&loader_path, loader_code)?;
    
    log::info!("Created loader stub");
    
    // Report: Creating loader
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::CreatingLoader).await;
    }
    
    // Convert binaries to object files using objcopy
    // When objcopy uses --input-target=binary, it treats the entire file as raw data
    // This means the complete ELF binary (including all sections like .license) 
    // is preserved as-is within the data blob that gets embedded
    log::info!("Converting binaries to object files (preserving all ELF sections as data)...");
    
    // Report: Converting to objects
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::ConvertingToObjects).await;
    }
    
    let binary_arch = base_info.arch.objcopy_binary();
    
    run_command(
        &compiler_config.objcopy,
        &[
            "--input-target", "binary",
            "--output-target", &compiler_config.objcopy_output,
            "--binary-architecture", binary_arch,
            "first", "first.o"
        ],
        work_dir_path
    )?;
    
    run_command(
        &compiler_config.objcopy,
        &[
            "--input-target", "binary",
            "--output-target", &compiler_config.objcopy_output,
            "--binary-architecture", binary_arch,
            "second", "second.o"
        ],
        work_dir_path
    )?;
    
    log::info!("✅ Binaries embedded as complete ELF files (all sections including .license preserved in data)");
    
    log::info!("Compiling loader stub...");
    
    // Report: Compiling loader
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::CompilingLoader).await;
    }
    
    // Compile loader stub with appropriate compiler
    run_command(
        &compiler_config.gcc,
        &["-c", "loader_stub.c", "-o", "loader.o"],
        work_dir_path
    )?;
    
    log::info!("Linking everything together...");
    
    // Report: Linking
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::Linking).await;
    }
    
    // Link everything with appropriate compiler
    let output_name = "merged_binary";
    run_command(
        &compiler_config.gcc,
        &[
            "loader.o",
            "first.o",
            "second.o",
            "-o",
            output_name,
        ],
        work_dir_path
    )?;
    
    let merged_path = work_dir_path.join(output_name);
    
    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&merged_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&merged_path, perms)?;
    }
    
    log::info!("✅ Linux ELF binary merged successfully");
    
    Ok(merged_path.to_string_lossy().to_string())
}

fn run_command(cmd: &str, args: &[&str], cwd: &Path) -> Result<()> {
    let output = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to execute {}", cmd))?;

    if !output.status.success() {
        anyhow::bail!(
            "{} failed: {}",
            cmd,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}
