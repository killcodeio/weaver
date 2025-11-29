use anyhow::{Result, bail, Context};
use std::path::Path;
use std::process::Command;
use std::fs;



/// The C loader stub template for Windows PE binaries
const WINDOWS_LOADER_STUB: &str = r#"
#include <windows.h>
#include <stdio.h>

// External symbols for embedded binaries
// Note: objcopy generates symbols based on the input filename. 
// Since we are linking against .exe files (e.g., first.exe), the generated symbols include '_exe_' in their names.
extern char _binary_first_exe_start[];
extern char _binary_first_exe_end[];
extern char _binary_second_exe_start[];
extern char _binary_second_exe_end[];

static int execute_binary(char* binary_data, size_t binary_size, const char* name, int wait_for_completion) {
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
    
    if (wait_for_completion) {
        WaitForSingleObject(pi.hProcess, INFINITE);
        
        DWORD exit_code;
        GetExitCodeProcess(pi.hProcess, &exit_code);
        
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
        DeleteFileA(temp_path);
        
        return exit_code;
    }
    
    CloseHandle(pi.hProcess);
    CloseHandle(pi.hThread);
    
    return 0;
}

int main(int argc, char** argv) {
    size_t first_size = _binary_first_exe_end - _binary_first_exe_start;
    size_t second_size = _binary_second_exe_end - _binary_second_exe_start;
    
    int sync_mode = SYNC_MODE_PLACEHOLDER;
    
    if (execute_binary(_binary_first_exe_start, first_size, "first", sync_mode) != 0) {
        fprintf(stderr, "Failed to execute first binary\n");
        return 1;
    }
    
    if (execute_binary(_binary_second_exe_start, second_size, "second", 1) != 0) {
        fprintf(stderr, "Failed to execute second binary\n");
        return 1;
    }
    
    return 0;
}
"#;

/// Merge two Windows PE binaries
pub fn merge_windows_pe(
    base_data: &[u8],
    overload_data: &[u8],
    mode: &str,
    sync: bool,
    work_path: &Path,
) -> Result<String> {
    log::info!("🪟 Merging Windows PE binaries...");
    
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
    
    // Determine order based on mode
    let (first_data, second_data) = match mode {
        "before" => (overload_data, base_data),  // Overload runs first
        "after" => (base_data, overload_data),   // Base runs first
        _ => (overload_data, base_data),
    };
    
    log::info!("Merge mode: {}, Sync: {}", mode, sync);
    
    // Write binaries to temp files
    let first_path = work_path.join("first.exe");
    let second_path = work_path.join("second.exe");
    
    fs::write(&first_path, first_data)?;
    fs::write(&second_path, second_data)?;
    
    log::info!("Wrote PE binaries: first={} bytes, second={} bytes", 
               first_data.len(), second_data.len());
    
    // Create Windows loader stub
    let loader_code = WINDOWS_LOADER_STUB.replace(
        "SYNC_MODE_PLACEHOLDER",
        if sync { "1" } else { "0" }
    );
    
    let loader_path = work_path.join("loader_stub.c");
    fs::write(&loader_path, loader_code)?;
    
    log::info!("Created Windows loader stub");
    
    // Convert PE binaries to object files using MinGW objcopy
    log::info!("Converting PE binaries to object files...");
    
    run_command(
        mingw_objcopy,
        &[
            "-I", "binary",
            "-O", "pe-x86-64",
            "-B", "i386:x86-64",
            "first.exe", "first.o"
        ],
        work_path
    )?;
    
    run_command(
        mingw_objcopy,
        &[
            "-I", "binary",
            "-O", "pe-x86-64",
            "-B", "i386:x86-64",
            "second.exe", "second.o"
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
            "first.o",
            "second.o",
            "-o",
            output_name,
            "-static",
        ],
        work_path
    )?;
    
    let merged_path = work_path.join(output_name);
    
    log::info!("✅ Windows PE binary merged successfully");
    
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    #[ignore] // Requires MinGW to be installed
    fn test_windows_pe_merge() {
        let temp_dir = TempDir::new().unwrap();
        
        // Simple PE header (MZ magic)
        let base_data = vec![0x4d, 0x5a];
        let overload_data = vec![0x4d, 0x5a];
        
        let result = merge_windows_pe(
            &base_data,
            &overload_data,
            "before",
            true,  // sync mode
            temp_dir.path(),
        );
        
        // Will fail without MinGW installed
        if result.is_err() {
            println!("Expected: MinGW not installed");
        }
    }
}
