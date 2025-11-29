use anyhow::{Result, bail, Context};
use std::path::Path;
use std::process::Command;
use std::fs;



/// V2 Windows loader stub with shared memory health monitoring
fn generate_windows_loader_v2(
    grace_period: u32,
    sync_mode: bool,
    network_failure_kill_count: u32,
) -> String {
    format!(r#"
#include <windows.h>
#include <stdio.h>
#include <time.h>

// Configuration (baked at compile time)
#define GRACE_PERIOD_SECONDS {grace_period}
#define SYNC_MODE {sync_mode_int}
#define NETWORK_FAILURE_KILL_COUNT {network_failure_kill_count}
#define HEALTH_CHECK_INTERVAL 5  // Check health every 5 seconds (in seconds)

// External symbols for embedded binaries
// Note: objcopy generates symbols based on the input filename. 
// Since we are linking against .exe files (e.g., base.exe), the generated symbols include '_exe_' in their names.
extern char _binary_base_exe_start[];
extern char _binary_base_exe_end[];
extern char _binary_overload_exe_start[];
extern char _binary_overload_exe_end[];

// Shared memory health status
typedef struct {{
    time_t last_success;           // Timestamp of last successful check
    int consecutive_failures;       // Counter of network failures
    int is_alive;                   // Heartbeat flag (1=alive, 0=dead)
    int should_kill_base;           // Signal from overload to kill base
    int parent_requests_kill;       // Signal from parent: kill yourself now
}} HealthStatus;

static HANDLE overload_process = NULL;
static HANDLE base_process = NULL;
static DWORD overload_pid = 0;
static DWORD base_pid = 0;
static HealthStatus* health_status = NULL;
static HANDLE health_shm_handle = NULL;
static volatile int health_check_running = 1;

// Monitor thread that checks overload health
static DWORD WINAPI health_monitor_thread(LPVOID arg) {{
    fprintf(stderr, "[KillCode] Health monitor started (grace_period=%ds, failure_threshold=%d)\n",
            GRACE_PERIOD_SECONDS, NETWORK_FAILURE_KILL_COUNT);
    
    while (health_check_running && base_process != NULL) {{
        Sleep(HEALTH_CHECK_INTERVAL * 1000);
        
        if (!health_status) continue;
        
        time_t now = time(NULL);
        time_t time_since_success = now - health_status->last_success;
        
        // Check 1: Grace period exceeded
        if (GRACE_PERIOD_SECONDS > 0 && time_since_success > GRACE_PERIOD_SECONDS) {{
            fprintf(stderr, "[KillCode] ⚠️  Grace period exceeded (%lld > %d seconds), killing base\n",
                    (long long)time_since_success, GRACE_PERIOD_SECONDS);
            TerminateProcess(base_process, 1);
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
            Sleep(1000);
            
            // Fallback: if still alive, kill base directly
            fprintf(stderr, "[KillCode] Fallback: Killing base directly\n");
            TerminateProcess(base_process, 1);
            break;
        }}
        
        // Check 3: Overload requested base kill
        if (health_status->should_kill_base) {{
            fprintf(stderr, "[KillCode] ⚠️  Overload requested base termination\n");
            TerminateProcess(base_process, 1);
            break;
        }}
        
        // Check 4: Overload is not alive (crashed/hung)
        if (health_status->is_alive == 0) {{
            fprintf(stderr, "[KillCode] ⚠️  Overload heartbeat lost, killing base\n");
            TerminateProcess(base_process, 1);
            break;
        }}
    }}
    
    return 0;
}}

static int execute_binary(char* binary_data, size_t binary_size, const char* name, int is_base) {{
    char temp_path[MAX_PATH];
    char temp_dir[MAX_PATH];
    
    GetTempPathA(MAX_PATH, temp_dir);
    sprintf(temp_path, "%s\\%s.exe", temp_dir, name);
    
    // Write binary to temp file
    HANDLE hFile = CreateFileA(temp_path, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (hFile == INVALID_HANDLE_VALUE) {{
        fprintf(stderr, "Failed to create temp file: %s\n", temp_path);
        return -1;
    }}
    
    DWORD written;
    if (!WriteFile(hFile, binary_data, binary_size, &written, NULL) || written != binary_size) {{
        fprintf(stderr, "Failed to write binary data\n");
        CloseHandle(hFile);
        return -1;
    }}
    CloseHandle(hFile);
    
    // Execute the binary
    STARTUPINFOA si = {{0}};
    PROCESS_INFORMATION pi = {{0}};
    si.cb = sizeof(si);
    
    if (!CreateProcessA(temp_path, NULL, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) {{
        fprintf(stderr, "Failed to execute: %s\n", temp_path);
        DeleteFileA(temp_path);
        return -1;
    }}
    
    if (!is_base) {{
        // This is the overload process
        overload_process = pi.hProcess;
        overload_pid = pi.dwProcessId;
        CloseHandle(pi.hThread);
        
        if (SYNC_MODE) {{
            // Sync mode: Wait for overload to complete verification
            fprintf(stderr, "[KillCode] Sync mode: Waiting for overload verification (PID: %lu)...\n", overload_pid);
            WaitForSingleObject(pi.hProcess, INFINITE);
            
            DWORD exit_code;
            GetExitCodeProcess(pi.hProcess, &exit_code);
            
            if (exit_code != 0) {{
                fprintf(stderr, "[KillCode] ❌ Overload verification failed (exit code: %lu)\n", exit_code);
                return -1;
            }}
            fprintf(stderr, "[KillCode] ✅ Overload verification successful\n");
        }} else {{
            // Async mode: Overload runs in background
            fprintf(stderr, "[KillCode] Async mode: Overload running in background (PID: %lu)\n", overload_pid);
        }}
        
        return 0;
    }} else {{
        // This is the base process
        base_process = pi.hProcess;
        base_pid = pi.dwProcessId;
        CloseHandle(pi.hThread);
        
        WaitForSingleObject(pi.hProcess, INFINITE);
        
        DWORD exit_code;
        GetExitCodeProcess(pi.hProcess, &exit_code);
        
        DeleteFileA(temp_path);
        
        // Base process completed - kill overload if still running
        if (overload_process != NULL) {{
            fprintf(stderr, "[KillCode] Base binary completed, terminating overload (PID: %lu)\n", overload_pid);
            TerminateProcess(overload_process, 0);
            
            // Clean up overload file
            char overload_path[MAX_PATH];
            sprintf(overload_path, "%s\\overload.exe", temp_dir);
            DeleteFileA(overload_path);
        }}
        
        return exit_code;
    }}
}}

int main(int argc, char** argv) {{
    size_t base_size = _binary_base_exe_end - _binary_base_exe_start;
    size_t overload_size = _binary_overload_exe_end - _binary_overload_exe_start;
    
    fprintf(stderr, "[KillCode] V2 Binary execution starting\n");
    fprintf(stderr, "[KillCode] Base: %zu bytes, Overload: %zu bytes\n", base_size, overload_size);
    fprintf(stderr, "[KillCode] Config: sync=%d, grace_period=%ds, failure_threshold=%d\n",
            SYNC_MODE, GRACE_PERIOD_SECONDS, NETWORK_FAILURE_KILL_COUNT);
    
    // Create shared memory for health monitoring (only in async mode)
    if (!SYNC_MODE && (GRACE_PERIOD_SECONDS > 0 || NETWORK_FAILURE_KILL_COUNT > 0)) {{
        char shm_name[64];
        sprintf(shm_name, "Local\\OverloadHealth_%lu", GetCurrentProcessId());
        
        health_shm_handle = CreateFileMappingA(
            INVALID_HANDLE_VALUE,
            NULL,
            PAGE_READWRITE,
            0,
            sizeof(HealthStatus),
            shm_name
        );
        
        if (health_shm_handle != NULL) {{
            health_status = (HealthStatus*)MapViewOfFile(
                health_shm_handle,
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                sizeof(HealthStatus)
            );
            
            if (health_status != NULL) {{
                // Initialize health status
                memset(health_status, 0, sizeof(HealthStatus));
                health_status->last_success = time(NULL);
                health_status->is_alive = 1;
                
                // Pass shared memory name to overload via env var
                SetEnvironmentVariableA("KILLCODE_HEALTH_SHM", shm_name);
                
                fprintf(stderr, "[KillCode] Health monitoring enabled: %s\n", shm_name);
            }} else {{
                fprintf(stderr, "[KillCode] Warning: Failed to map shared memory\n");
            }}
        }} else {{
            fprintf(stderr, "[KillCode] Warning: Failed to create shared memory\n");
        }}
    }}
    
    // Start overload
    if (execute_binary(_binary_overload_exe_start, overload_size, "overload", 0) != 0) {{
        fprintf(stderr, "[KillCode] Failed to start overload binary\n");
        return 1;
    }}
    
    // In async mode with health monitoring, start monitor thread
    HANDLE monitor_thread = NULL;
    if (!SYNC_MODE && health_status && 
        (GRACE_PERIOD_SECONDS > 0 || NETWORK_FAILURE_KILL_COUNT > 0)) {{
        monitor_thread = CreateThread(NULL, 0, health_monitor_thread, NULL, 0, NULL);
        if (monitor_thread == NULL) {{
            fprintf(stderr, "[KillCode] Warning: Failed to create health monitor thread\n");
        }}
    }}
    
    // Execute base binary
    fprintf(stderr, "[KillCode] Starting base binary...\n");
    int base_exit = execute_binary(_binary_base_exe_start, base_size, "base", 1);
    
    // Stop health monitor
    health_check_running = 0;
    if (monitor_thread != NULL) {{
        WaitForSingleObject(monitor_thread, INFINITE);
        CloseHandle(monitor_thread);
    }}
    
    // Cleanup shared memory
    if (health_status != NULL) {{
        UnmapViewOfFile(health_status);
    }}
    if (health_shm_handle != NULL) {{
        CloseHandle(health_shm_handle);
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

/// V2 merge for Windows with health monitoring
pub fn merge_windows_pe_v2_stop_on_exit(
    base_data: &[u8],
    overload_data: &[u8],
    work_path: &Path,
    grace_period: u32,
    sync_mode: bool,
    network_failure_kill_count: u32,
) -> Result<String> {
    log::info!("🪟 V2 Merging Windows PE binaries with health monitoring...");
    
    // Check if MinGW cross-compiler is available
    let mingw_gcc = "x86_64-w64-mingw32-gcc";
    let mingw_objcopy = "x86_64-w64-mingw32-objcopy";
    
    if !is_command_available(mingw_gcc) {
        bail!(
            "MinGW cross-compiler not found: {}\n\
             Install with: apt-get install mingw-w64 gcc-mingw-w64-x86-64",
            mingw_gcc
        );
    }
    
    // Write binaries to disk
    let base_path = work_path.join("base.exe");
    let overload_path = work_path.join("overload.exe");
    
    fs::write(&base_path, base_data)
        .context("Failed to write base binary")?;
    fs::write(&overload_path, overload_data)
        .context("Failed to write overload binary")?;
    
    // Generate V2 C loader with baked config
    let loader_stub = generate_windows_loader_v2(grace_period, sync_mode, network_failure_kill_count);
    let loader_c_path = work_path.join("loader.c");
    fs::write(&loader_c_path, loader_stub)
        .context("Failed to write loader stub")?;
    
    log::info!("📝 Generated V2 loader stub with config");
    
    // Convert binaries to object files
    log::info!("Converting PE binaries to object files...");
    
    run_command(
        mingw_objcopy,
        &[
            "-I", "binary",
            "-O", "pe-x86-64",
            "-B", "i386:x86-64",
            "base.exe",
            "base.o",
        ],
        work_path
    )?;
    
    run_command(
        mingw_objcopy,
        &[
            "-I", "binary",
            "-O", "pe-x86-64",
            "-B", "i386:x86-64",
            "overload.exe",
            "overload.o",
        ],
        work_path
    )?;
    
    log::info!("🔨 Compiling with MinGW...");
    
    // Compile and link everything
    let output_path = work_path.join("merged.exe");
    run_command(
        mingw_gcc,
        &[
            "-o", "merged.exe",
            "loader.c",
            "base.o",
            "overload.o",
            "-static",
        ],
        work_path
    )?;
    
    log::info!("✅ V2 merge completed successfully");
    
    Ok(output_path.to_string_lossy().to_string())
}

fn is_command_available(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn run_command(cmd: &str, args: &[&str], cwd: &Path) -> Result<()> {
    let output = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("Failed to execute {}", cmd))?;

    if !output.status.success() {
        bail!(
            "{} failed: {}",
            cmd,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}
