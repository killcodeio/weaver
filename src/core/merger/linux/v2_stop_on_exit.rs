use anyhow::{Result, Context};
use std::process::Command;
use std::path::Path;
use std::fs;

use crate::core::binary::{BinaryInfo, CompilerConfig};
use crate::core::progress::{ProgressTracker, ProgressStep};

/// V2 C loader stub with shared memory health monitoring
/// This version supports:
/// - grace_period: Network timeout tolerance
/// - sync_mode: Wait for overload verification before starting base
/// - network_failure_kill_count: Kill base after N consecutive failures
fn generate_loader_stub_v2(
    grace_period: u32,
    sync_mode: bool,
    network_failure_kill_count: u32,
) -> String {
    format!(r#"
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
#include <time.h>
#include <pthread.h>

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif

// Configuration (baked at compile time)
#define GRACE_PERIOD_SECONDS {grace_period}
#define SYNC_MODE {sync_mode_int}
#define NETWORK_FAILURE_KILL_COUNT {network_failure_kill_count}
#define HEALTH_CHECK_INTERVAL 5  // Check health every 5 seconds

extern char _binary_base_start[];
extern char _binary_base_end[];
extern char _binary_overload_start[];
extern char _binary_overload_end[];

// Shared memory health status
typedef struct {{
    time_t last_success;           // Timestamp of last successful check
    int consecutive_failures;       // Counter of network failures
    int is_alive;                   // Heartbeat flag (1=alive, 0=dead)
    int should_kill_base;           // Signal from overload to kill base
    int parent_requests_kill;       // Signal from parent: kill yourself now
}} HealthStatus;

static pid_t overload_pid = 0;
static pid_t base_pid = 0;
static HealthStatus* health_status = NULL;
static int health_check_running = 1;

// Monitor thread that checks overload health
static void* health_monitor_thread(void* arg) {{
    fprintf(stderr, "[KillCode] Health monitor started (grace_period=%ds, failure_threshold=%d)\n",
            GRACE_PERIOD_SECONDS, NETWORK_FAILURE_KILL_COUNT);
    
    while (health_check_running && base_pid > 0) {{
        sleep(HEALTH_CHECK_INTERVAL);
        
        if (!health_status) continue;
        
        time_t now = time(NULL);
        time_t time_since_success = now - health_status->last_success;
        
        // Check 1: Grace period exceeded
        if (GRACE_PERIOD_SECONDS > 0 && time_since_success > GRACE_PERIOD_SECONDS) {{
            fprintf(stderr, "[KillCode] ⚠️  Grace period exceeded (%ld > %d seconds), killing base\n",
                    time_since_success, GRACE_PERIOD_SECONDS);
            kill(base_pid, SIGTERM);
            usleep(100000);
            kill(base_pid, SIGKILL);
            break;
        }}
        
        // Check 2: Network failure threshold exceeded
        if (NETWORK_FAILURE_KILL_COUNT > 0 && 
            health_status->consecutive_failures >= NETWORK_FAILURE_KILL_COUNT) {{
            fprintf(stderr, "[KillCode] ⚠️  Network failure threshold exceeded (%d/%d), signaling overload to kill parent\n",
                    health_status->consecutive_failures, NETWORK_FAILURE_KILL_COUNT);
            
            // Signal overload to execute kill method on parent
            health_status->parent_requests_kill = 1;
            
            // Wait a moment for overload to execute kill
            sleep(1);
            
            // Fallback: if still alive, kill base directly
            fprintf(stderr, "[KillCode] Fallback: Killing base directly\n");
            kill(base_pid, SIGTERM);
            usleep(100000);
            kill(base_pid, SIGKILL);
            break;
        }}
        
        // Check 3: Overload requested base kill
        if (health_status->should_kill_base) {{
            fprintf(stderr, "[KillCode] ⚠️  Overload requested base termination\n");
            kill(base_pid, SIGTERM);
            usleep(100000);
            kill(base_pid, SIGKILL);
            break;
        }}
        
        // Check 4: Overload is not alive (crashed/hung)
        if (health_status->is_alive == 0) {{
            fprintf(stderr, "[KillCode] ⚠️  Overload heartbeat lost, killing base\n");
            kill(base_pid, SIGTERM);
            usleep(100000);
            kill(base_pid, SIGKILL);
            break;
        }}
    }}
    
    return NULL;
}}

static int execute_binary(char* binary_data, size_t binary_size, const char* name, int is_base) {{
    int fd = memfd_create(name, MFD_CLOEXEC);
    if (fd < 0) {{
        perror("memfd_create failed");
        return -1;
    }}
    
    if (write(fd, binary_data, binary_size) != (ssize_t)binary_size) {{
        perror("Failed to write binary");
        close(fd);
        return -1;
    }}
    
    pid_t pid = fork();
    if (pid < 0) {{
        perror("fork failed");
        close(fd);
        return -1;
    }}
    
    if (pid == 0) {{
        // Child process
        char fd_path[32];
        snprintf(fd_path, sizeof(fd_path), "/proc/self/fd/%d", fd);
        execl(fd_path, name, NULL);
        perror("execl failed");
        _exit(1);
    }}
    
    // Parent process
    if (!is_base) {{
        // This is the overload process
        overload_pid = pid;
        
        if (SYNC_MODE) {{
            // Sync mode: Wait for overload to complete verification before continuing
            fprintf(stderr, "[KillCode] Sync mode: Waiting for overload verification (PID: %d)...\n", overload_pid);
            int status;
            waitpid(pid, &status, 0);
            close(fd);
            
            if (WIFEXITED(status)) {{
                int exit_code = WEXITSTATUS(status);
                if (exit_code != 0) {{
                    fprintf(stderr, "[KillCode] ❌ Overload verification failed (exit code: %d)\n", exit_code);
                    return -1;
                }}
                fprintf(stderr, "[KillCode] ✅ Overload verification successful\n");
            }} else {{
                fprintf(stderr, "[KillCode] ❌ Overload terminated abnormally\n");
                return -1;
            }}
        }} else {{
            // Async mode: Overload runs in background
            fprintf(stderr, "[KillCode] Async mode: Overload running in background (PID: %d)\n", overload_pid);
            close(fd);
        }}
        
        return 0;
    }} else {{
        // This is the base process
        base_pid = pid;
        int status;
        waitpid(pid, &status, 0);
        close(fd);
        
        // Base process completed - kill overload if still running
        if (overload_pid > 0) {{
            fprintf(stderr, "[KillCode] Base binary completed, terminating overload (PID: %d)\n", overload_pid);
            kill(overload_pid, SIGTERM);
            usleep(100000);
            
            int kill_status;
            if (waitpid(overload_pid, &kill_status, WNOHANG) == 0) {{
                fprintf(stderr, "[KillCode] Forcing SIGKILL on overload\n");
                kill(overload_pid, SIGKILL);
                waitpid(overload_pid, &kill_status, 0);
            }}
        }}
        
        return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    }}
}}

int main(int argc, char** argv) {{
    size_t base_size = _binary_base_end - _binary_base_start;
    size_t overload_size = _binary_overload_end - _binary_overload_start;
    
    fprintf(stderr, "[KillCode] V2 Binary execution starting\n");
    fprintf(stderr, "[KillCode] Base: %zu bytes, Overload: %zu bytes\n", base_size, overload_size);
    fprintf(stderr, "[KillCode] Config: sync=%d, grace_period=%ds, failure_threshold=%d\n",
            SYNC_MODE, GRACE_PERIOD_SECONDS, NETWORK_FAILURE_KILL_COUNT);
    
    // Create shared memory for health monitoring (only in async mode)
    if (!SYNC_MODE && (GRACE_PERIOD_SECONDS > 0 || NETWORK_FAILURE_KILL_COUNT > 0)) {{
        char shm_name[64];
        snprintf(shm_name, sizeof(shm_name), "/overload_health_%d", getpid());
        
        int shm_fd = shm_open(shm_name, O_CREAT | O_RDWR, 0600);
        if (shm_fd >= 0) {{
            ftruncate(shm_fd, sizeof(HealthStatus));
            health_status = mmap(NULL, sizeof(HealthStatus), PROT_READ | PROT_WRITE,
                                MAP_SHARED, shm_fd, 0);
            
            if (health_status != MAP_FAILED) {{
                // Initialize health status
                memset(health_status, 0, sizeof(HealthStatus));
                health_status->last_success = time(NULL);
                health_status->is_alive = 1;
                
                // Pass shared memory name to overload via env var
                setenv("KILLCODE_HEALTH_SHM", shm_name, 1);
                
                fprintf(stderr, "[KillCode] Health monitoring enabled: %s\n", shm_name);
            }} else {{
                fprintf(stderr, "[KillCode] Warning: Failed to map shared memory\n");
                health_status = NULL;
            }}
            close(shm_fd);
        }} else {{
            fprintf(stderr, "[KillCode] Warning: Failed to create shared memory\n");
        }}
    }}
    
    // Start overload
    if (execute_binary(_binary_overload_start, overload_size, "overload", 0) != 0) {{
        fprintf(stderr, "[KillCode] Failed to start overload binary\n");
        return 1;
    }}
    
    // In async mode with health monitoring, start monitor thread
    pthread_t monitor_tid;
    if (!SYNC_MODE && health_status && 
        (GRACE_PERIOD_SECONDS > 0 || NETWORK_FAILURE_KILL_COUNT > 0)) {{
        if (pthread_create(&monitor_tid, NULL, health_monitor_thread, NULL) != 0) {{
            fprintf(stderr, "[KillCode] Warning: Failed to create health monitor thread\n");
        }}
    }}
    
    // Execute base binary
    fprintf(stderr, "[KillCode] Starting base binary...\n");
    int base_exit = execute_binary(_binary_base_start, base_size, "base", 1);
    
    // Stop health monitor
    health_check_running = 0;
    if (!SYNC_MODE && health_status) {{
        pthread_join(monitor_tid, NULL);
    }}
    
    fprintf(stderr, "[KillCode] Base binary exited with code: %d\n", base_exit);
    
    return base_exit;
}}
"#,
        grace_period = grace_period,
        sync_mode_int = if sync_mode { 1 } else { 0 },
        network_failure_kill_count = network_failure_kill_count,
    )
}

/// V2 merge with health monitoring support
pub async fn merge_linux_elf_v2_stop_on_exit(
    base_data: &[u8],
    overload_data: &[u8],
    work_dir_path: &Path,
    base_info: &BinaryInfo,
    task_id: &str,
    grace_period: u32,
    sync_mode: bool,
    network_failure_kill_count: u32,
) -> Result<String> {
    log::info!("🐧 V2 Merging Linux ELF binaries with health monitoring...");
    
    // Initialize progress tracker
    let progress_tracker = if !task_id.is_empty() {
        match ProgressTracker::new("redis://redis:6379", task_id.to_string()) {
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
    
    // Get compiler configuration
    let compiler_config = CompilerConfig::for_binary(base_info);
    
    if !compiler_config.is_available() {
        anyhow::bail!(
            "❌ Required compiler not found: {}. Please install the appropriate cross-compiler.",
            compiler_config.gcc
        );
    }
    
    // Write binaries to disk
    let base_path = work_dir_path.join("base");
    let overload_path = work_dir_path.join("overload");
    
    fs::write(&base_path, base_data)
        .context("Failed to write base binary")?;
    fs::write(&overload_path, overload_data)
        .context("Failed to write overload binary")?;
    
    // Generate V2 C loader with baked config
    let loader_stub = generate_loader_stub_v2(grace_period, sync_mode, network_failure_kill_count);
    let loader_c_path = work_dir_path.join("loader.c");
    fs::write(&loader_c_path, loader_stub)
        .context("Failed to write loader stub")?;
    
    log::info!("📝 Generated V2 loader stub with config");
    
    // Report: Compiling wrapper
    if let Some(ref tracker) = progress_tracker {
        let _ = tracker.update(ProgressStep::CompilingLoader).await;
    }
    
    // Compile with objcopy to embed binaries
    let loader_o_path = work_dir_path.join("loader.o");
    let base_o_path = work_dir_path.join("base.o");
    let overload_o_path = work_dir_path.join("overload.o");
    let output_path = work_dir_path.join("merged");
    
    // Convert binaries to object files
    let binary_arch = base_info.arch.objcopy_binary();
    let base_objcopy = Command::new(&compiler_config.objcopy)
        .args(&[
            "--input-target", "binary",
            "--output-target", &compiler_config.objcopy_output,
            "--binary-architecture", binary_arch,
            "base",
            "base.o",
        ])
        .current_dir(&work_dir_path)
        .output()
        .context("Failed to run objcopy for base")?;
    
    if !base_objcopy.status.success() {
        anyhow::bail!("objcopy failed for base: {}", String::from_utf8_lossy(&base_objcopy.stderr));
    }
    
    let overload_objcopy = Command::new(&compiler_config.objcopy)
        .args(&[
            "--input-target", "binary",
            "--output-target", &compiler_config.objcopy_output,
            "--binary-architecture", binary_arch,
            "overload",
            "overload.o",
        ])
        .current_dir(&work_dir_path)
        .output()
        .context("Failed to run objcopy for overload")?;
    
    if !overload_objcopy.status.success() {
        anyhow::bail!("objcopy failed for overload: {}", String::from_utf8_lossy(&overload_objcopy.stderr));
    }
    
    // Compile loader.c
    let gcc_args = vec![
        "-o", "merged",
        "loader.c",
        "base.o",
        "overload.o",
        "-lpthread",  // Required for health monitor thread
    ];
    
    log::info!("🔨 Compiling with: {} {}", compiler_config.gcc, gcc_args.join(" "));
    
    let gcc_output = Command::new(&compiler_config.gcc)
        .args(&gcc_args)
        .current_dir(&work_dir_path)
        .output()
        .context("Failed to run gcc")?;
    
    if !gcc_output.status.success() {
        anyhow::bail!("gcc compilation failed: {}", String::from_utf8_lossy(&gcc_output.stderr));
    }
    
    log::info!("✅ V2 merge completed successfully");
    
    Ok(output_path.to_string_lossy().to_string())
}
