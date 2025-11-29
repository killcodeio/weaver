use anyhow::{Result, bail, Context};
use std::path::Path;
use std::process::Command;
use std::fs;



/// Enhanced Windows loader stub that kills overload when base exits
const WINDOWS_LOADER_STUB_STOP_ON_EXIT: &str = r#"
#include <windows.h>
#include <stdio.h>

// External symbols for embedded binaries
// Note: objcopy generates symbols based on the input filename. 
// Since we are linking against .exe files (e.g., base.exe), the generated symbols include '_exe_' in their names.
extern char _binary_base_exe_start[];
extern char _binary_base_exe_end[];
extern char _binary_overload_exe_start[];
extern char _binary_overload_exe_end[];

static HANDLE overload_process = NULL;
static DWORD overload_pid = 0;

static int execute_binary(char* binary_data, size_t binary_size, const char* name, int is_base) {
    // Create a temporary file
    char temp_path[MAX_PATH];
    char temp_dir[MAX_PATH];
    
    GetTempPathA(MAX_PATH, temp_dir);
    sprintf(temp_path, "%s\\%s.exe", temp_dir, name);
    
    // Write binary to temp file
    HANDLE hFile = CreateFileA(temp_path, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (hFile == INVALID_HANDLE_VALUE) {
        fprintf(stderr, "Failed to create temp file: %s\n", temp_path);
        return -1;
    }
    
    DWORD written;
    if (!WriteFile(hFile, binary_data, binary_size, &written, NULL) || written != binary_size) {
        fprintf(stderr, "Failed to write binary data\n");
        CloseHandle(hFile);
        return -1;
    }
    CloseHandle(hFile);
    
    // Execute the binary
    STARTUPINFOA si = {0};
    PROCESS_INFORMATION pi = {0};
    si.cb = sizeof(si);
    
    if (!CreateProcessA(temp_path, NULL, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) {
        fprintf(stderr, "Failed to execute: %s\n", temp_path);
        DeleteFileA(temp_path);
        return -1;
    }
    
    if (!is_base) {
        // This is the overload process - store handle and PID
        overload_process = pi.hProcess;
        overload_pid = pi.dwProcessId;
        CloseHandle(pi.hThread); // We don't need the thread handle
        
        // Don't delete file yet, it's running
        return 0;
    } else {
        // This is the base process - wait for it to complete
        WaitForSingleObject(pi.hProcess, INFINITE);
        
        DWORD exit_code;
        GetExitCodeProcess(pi.hProcess, &exit_code);
        
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
        DeleteFileA(temp_path);
        
        // Base process completed - kill overload if it's still running
        if (overload_process != NULL) {
            fprintf(stderr, "[KillCode] Base binary completed, terminating overload process (PID: %lu)\n", overload_pid);
            TerminateProcess(overload_process, 0);
            CloseHandle(overload_process);
            
            // Clean up overload file
            char overload_path[MAX_PATH];
            sprintf(overload_path, "%s\\overload.exe", temp_dir);
            DeleteFileA(overload_path);
        }
        
        return exit_code;
    }
}

int main(int argc, char** argv) {
    size_t base_size = _binary_base_exe_end - _binary_base_exe_start;
    size_t overload_size = _binary_overload_exe_end - _binary_overload_exe_start;
    
    // Start overload first (non-blocking)
    if (execute_binary(_binary_overload_exe_start, overload_size, "overload", 0) != 0) {
        fprintf(stderr, "[KillCode] Failed to start overload binary\n");
        return 1;
    }
    
    // Execute base and wait for completion
    int base_exit = execute_binary(_binary_base_exe_start, base_size, "base", 1);
    
    return base_exit;
}
"#;

/// Merge two Windows PE binaries with stop-on-exit logic
pub fn merge_windows_pe_stop_on_exit(
    base_data: &[u8],
    overload_data: &[u8],
    work_path: &Path,
) -> Result<String> {
    log::info!("🪟 Merging Windows PE binaries with stop-on-exit mode...");
    
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
    
    log::info!("✅ Using MinGW compiler: {}", mingw_gcc);
    
    // Write binaries to temp files
    let base_path = work_path.join("base.exe");
    let overload_path = work_path.join("overload.exe");
    
    fs::write(&base_path, base_data)?;
    fs::write(&overload_path, overload_data)?;
    
    log::info!("Wrote PE binaries: base={} bytes, overload={} bytes", 
               base_data.len(), overload_data.len());
    
    // Create Windows loader stub
    let loader_path = work_path.join("loader_stub.c");
    fs::write(&loader_path, WINDOWS_LOADER_STUB_STOP_ON_EXIT)?;
    
    log::info!("Created Windows stop-on-exit loader stub");
    
    // Convert PE binaries to object files using MinGW objcopy
    log::info!("Converting PE binaries to object files...");
    
    run_command(
        mingw_objcopy,
        &[
            "-I", "binary",
            "-O", "pe-x86-64",
            "-B", "i386:x86-64",
            "base.exe", "base.o"
        ],
        work_path
    )?;
    
    run_command(
        mingw_objcopy,
        &[
            "-I", "binary",
            "-O", "pe-x86-64",
            "-B", "i386:x86-64",
            "overload.exe", "overload.o"
        ],
        work_path
    )?;
    
    log::info!("Compiling Windows loader stub...");
    
    // Compile loader stub with MinGW
    run_command(
        mingw_gcc,
        &["-c", "loader_stub.c", "-o", "loader.o"],
        work_path
    )?;
    
    log::info!("Linking Windows PE binary...");
    
    // Link everything with MinGW
    let output_name = "merged.exe";
    run_command(
        mingw_gcc,
        &[
            "loader.o",
            "base.o",
            "overload.o",
            "-o",
            output_name,
            "-static",
        ],
        work_path
    )?;
    
    let merged_path = work_path.join(output_name);
    
    log::info!("✅ Windows PE binary merged successfully with stop-on-exit");
    
    Ok(merged_path.to_string_lossy().to_string())
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
