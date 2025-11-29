use anyhow::{Result, Context};
use std::process::Command;
use std::path::Path;
use std::fs;

use crate::core::binary::{BinaryInfo, CompilerConfig};
use crate::core::progress::{ProgressTracker, ProgressStep};

/// Enhanced C loader stub template that kills overload when base exits
const LOADER_STUB_STOP_ON_EXIT: &str = r#"
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
#include <signal.h>

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif

extern char _binary_base_start[];
extern char _binary_base_end[];
extern char _binary_overload_start[];
extern char _binary_overload_end[];

static pid_t overload_pid = 0;

static int execute_binary(char* binary_data, size_t binary_size, const char* name, int is_base) {
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
        // Child process
        char fd_path[32];
        snprintf(fd_path, sizeof(fd_path), "/proc/self/fd/%d", fd);
        execl(fd_path, name, NULL);
        perror("execl failed");
        _exit(1);
    }
    
    // Parent process
    if (!is_base) {
        // This is the overload process - store PID
        overload_pid = pid;
        close(fd);
        return 0;
    } else {
        // This is the base process - wait for it to complete
        int status;
        waitpid(pid, &status, 0);
        close(fd);
        
        // Base process completed - kill overload if it's still running
        if (overload_pid > 0) {
            fprintf(stderr, "[KillCode] Base binary completed, terminating overload process (PID: %d)\n", overload_pid);
            kill(overload_pid, SIGTERM);
            
            // Give it a moment to terminate gracefully
            usleep(100000); // 100ms
            
            // Force kill if still running
            int kill_status;
            if (waitpid(overload_pid, &kill_status, WNOHANG) == 0) {
                fprintf(stderr, "[KillCode] Overload didn't terminate, forcing SIGKILL\n");
                kill(overload_pid, SIGKILL);
                waitpid(overload_pid, &kill_status, 0);
            }
        }
        
        return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    }
}

int main(int argc, char** argv) {
    size_t base_size = _binary_base_end - _binary_base_start;
    size_t overload_size = _binary_overload_end - _binary_overload_start;
    
    fprintf(stderr, "[KillCode] Starting binary execution: base=%zu bytes, overload=%zu bytes\n", 
            base_size, overload_size);
    
    // Start overload first (non-blocking)
    if (execute_binary(_binary_overload_start, overload_size, "overload", 0) != 0) {
        fprintf(stderr, "[KillCode] Failed to start overload binary\n");
        return 1;
    }
    
    fprintf(stderr, "[KillCode] Overload started (PID: %d)\n", overload_pid);
    
    // Execute base and wait for completion
    fprintf(stderr, "[KillCode] Starting base binary...\n");
    int base_exit = execute_binary(_binary_base_start, base_size, "base", 1);
    
    fprintf(stderr, "[KillCode] Base binary exited with code: %d\n", base_exit);
    
    return base_exit;
}
"#;

/// Merge two Linux ELF binaries with base exit detection
pub async fn merge_linux_elf_stop_on_exit(
    base_data: &[u8],
    overload_data: &[u8],
    work_dir_path: &Path,
    base_info: &BinaryInfo,
    task_id: &str,
) -> Result<String> {
    log::info!("🐧 Merging Linux ELF binaries with stop-on-exit mode...");
    
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
        None
    };
    
    // Report: Detecting platforms
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::DetectingPlatforms).await;
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
    
    // Write binaries to temp files with explicit names
    let base_path = work_dir_path.join("base");
    let overload_path = work_dir_path.join("overload");
    
    // Report: Writing binaries
    fs::write(&base_path, base_data)?;
    fs::write(&overload_path, overload_data)?;
    
    log::info!("Wrote binaries: base={} bytes, overload={} bytes", 
               base_data.len(), overload_data.len());
    
    // Create loader stub with stop-on-exit logic
    let loader_path = work_dir_path.join("loader_stub.c");
    fs::write(&loader_path, LOADER_STUB_STOP_ON_EXIT)?;
    
    log::info!("Created stop-on-exit loader stub");
    
    // Report: Creating loader
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::CreatingLoader).await;
    }
    
    // Convert binaries to object files using objcopy
    log::info!("Converting binaries to object files...");
    
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
            "base", "base.o"
        ],
        work_dir_path
    )?;
    
    run_command(
        &compiler_config.objcopy,
        &[
            "--input-target", "binary",
            "--output-target", &compiler_config.objcopy_output,
            "--binary-architecture", binary_arch,
            "overload", "overload.o"
        ],
        work_dir_path
    )?;
    
    log::info!("✅ Binaries embedded as complete ELF files");
    
    log::info!("Compiling loader stub...");
    
    // Report: Compiling loader
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::CompilingLoader).await;
    }
    
    // Compile loader stub
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
    
    // Link everything
    let output_name = "merged_binary";
    run_command(
        &compiler_config.gcc,
        &[
            "loader.o",
            "base.o",
            "overload.o",
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
    
    log::info!("✅ Linux ELF binary merged successfully with stop-on-exit");
    
    Ok(merged_path.to_string_lossy().to_string())
}

fn run_command(cmd: &str, args: &[&str], cwd: &Path) -> Result<()> {
    let output = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .context(format!("Failed to execute: {} {:?}", cmd, args))?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Command failed: {} {:?}\nError: {}",
            cmd,
            args,
            stderr
        );
    }
    
    Ok(())
}
