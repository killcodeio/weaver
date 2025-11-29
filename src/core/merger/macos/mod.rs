use anyhow::{Result, bail, Context};
use std::path::Path;
use std::process::Command;
use std::fs;

/// The C loader stub template for macOS Mach-O binaries
const MACOS_LOADER_STUB: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
#include <sys/stat.h>

// External symbols for embedded binaries
extern char _binary_first_start[];
extern char _binary_first_end[];
extern char _binary_second_start[];
extern char _binary_second_end[];

static int execute_binary(char* binary_data, size_t binary_size, const char* name, int wait_for_completion) {
    // Create temporary file
    char temp_path[256];
    snprintf(temp_path, sizeof(temp_path), "/tmp/%s", name);
    
    // Write binary to temp file
    FILE* f = fopen(temp_path, "wb");
    if (!f) {
        fprintf(stderr, "Failed to create temp file: %s\n", temp_path);
        return -1;
    }
    
    if (fwrite(binary_data, 1, binary_size, f) != binary_size) {
        fprintf(stderr, "Failed to write binary data\n");
        fclose(f);
        return -1;
    }
    fclose(f);
    
    // Make executable
    chmod(temp_path, 0755);
    
    // Fork and execute
    pid_t pid = fork();
    if (pid < 0) {
        fprintf(stderr, "fork failed\n");
        unlink(temp_path);
        return -1;
    }
    
    if (pid == 0) {
        // Child process
        execl(temp_path, name, NULL);
        fprintf(stderr, "execl failed\n");
        _exit(1);
    }
    
    // Parent process
    if (wait_for_completion) {
        int status;
        waitpid(pid, &status, 0);
        unlink(temp_path);
        return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    }
    
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

/// Merge two macOS Mach-O binaries
/// 
/// This implementation uses osxcross if available, otherwise provides
/// helpful error message with instructions for native macOS merging
/// 
/// Mach-O (Mach Object) is the executable format used by macOS, iOS, and other Apple platforms.
/// 
/// Key differences from ELF and PE:
/// - Different segment/section structure (LC_SEGMENT_64 load commands)
/// - Fat binaries (universal binaries) support multiple architectures
/// - Code signing requirements on modern macOS
/// - Different dynamic linker (dyld)
/// 
/// Implementation approach:
/// 1. Install macOS cross-compilation tools:
///    - osxcross (cross-compiler for macOS on Linux)
///    - OR use native macOS tools if running on macOS
/// 
/// 2. Parse Mach-O format:
///    - Use goblin library (already integrated) for parsing
///    - Extract load commands and segments
///    - Identify entry point
/// 
/// 3. Create Mach-O loader stub:
///    - Similar concept to ELF loader
///    - Handle dyld initialization
///    - Call both binaries in sequence
/// 
/// 4. Use appropriate tools:
///    - x86_64-apple-darwin-objcopy (if available via osxcross)
///    - x86_64-apple-darwin-gcc for compilation
///    - OR use LIEF library for direct binary manipulation
/// 
/// 5. Handle code signing:
///    - Modern macOS requires code signing
///    - May need to use ad-hoc signing: codesign -s - binary
///    - Or skip signing for testing purposes
/// 
/// Alternative approach using LIEF:
/// - LIEF library supports Mach-O manipulation
/// - Can inject code directly without loader stub
/// - See bin-utils project for LIEF integration example
/// 
/// Note: macOS binary merging is complex due to:
/// - Code signing requirements
/// - System Integrity Protection (SIP)
/// - Notarization requirements for distribution
/// - Limited cross-compilation support from Linux
/// 
/// Recommended: Start with simple console apps, test on actual macOS hardware
pub fn merge_macos_macho(
    base_data: &[u8],
    overload_data: &[u8],
    mode: &str,
    sync: bool,
    work_path: &Path,
) -> Result<String> {
    log::info!("🍎 Merging macOS Mach-O binaries...");
    
    // Check if osxcross is available
    let osxcross_gcc = "x86_64-apple-darwin21.4-gcc";
    let osxcross_objcopy = "x86_64-apple-darwin21.4-objcopy";
    
    // Try to find osxcross tools
    let has_osxcross = is_command_available(osxcross_gcc);
    
    if !has_osxcross {
        // Fallback: Check for native macOS tools
        let has_native_clang = is_command_available("clang");
        
        if !has_native_clang {
            bail!(
                "macOS Mach-O binary merging requires either:\n\
                 \n\
                 Option 1: osxcross (cross-compile from Linux)\n\
                 - Install: See MACOS_IMPLEMENTATION.md\n\
                 - Tools needed: {}\n\
                 \n\
                 Option 2: Native macOS tools\n\
                 - Install Xcode Command Line Tools\n\
                 - Run: xcode-select --install\n\
                 \n\
                 Option 3: LIEF library (coming soon)\n\
                 - Direct binary manipulation\n\
                 - Works on any platform\n\
                 \n\
                 Current status: No macOS toolchain detected",
                osxcross_gcc
            );
        }
        
        // Use native macOS tools
        log::info!("Using native macOS tools (clang)");
        return merge_macos_native(base_data, overload_data, mode, sync, work_path);
    }
    
    // Use osxcross
    log::info!("✅ Using osxcross: {}", osxcross_gcc);
    merge_macos_osxcross(base_data, overload_data, mode, sync, work_path, osxcross_gcc, osxcross_objcopy)
}

/// Merge using osxcross toolchain
fn merge_macos_osxcross(
    base_data: &[u8],
    overload_data: &[u8],
    mode: &str,
    sync: bool,
    work_path: &Path,
    gcc: &str,
    objcopy: &str,
) -> Result<String> {
    log::info!("🍎 Merging macOS Mach-O binaries with osxcross...");
    
    // Determine order based on mode
    let (first_data, second_data) = match mode {
        "before" => (overload_data, base_data),  // Overload runs first
        "after" => (base_data, overload_data),   // Base runs first
        _ => (overload_data, base_data),
    };
    
    log::info!("Merge mode: {}, Sync: {}", mode, sync);
    
    // Write binaries to temp files
    let first_path = work_path.join("first");
    let second_path = work_path.join("second");
    
    fs::write(&first_path, first_data)?;
    fs::write(&second_path, second_data)?;
    
    log::info!("Wrote Mach-O binaries: first={} bytes, second={} bytes", 
               first_data.len(), second_data.len());
    
    // Create macOS loader stub
    let loader_code = MACOS_LOADER_STUB.replace(
        "SYNC_MODE_PLACEHOLDER",
        if sync { "1" } else { "0" }
    );
    
    let loader_path = work_path.join("loader_stub.c");
    fs::write(&loader_path, loader_code)?;
    
    log::info!("Created macOS loader stub");
    
    // Convert Mach-O binaries to object files
    log::info!("Converting Mach-O binaries to object files...");
    
    run_command(
        objcopy,
        &[
            "-I", "binary",
            "-O", "mach-o-x86-64",
            "-B", "i386:x86-64",
            "first", "first.o"
        ],
        work_path
    )?;
    
    run_command(
        objcopy,
        &[
            "-I", "binary",
            "-O", "mach-o-x86-64",
            "-B", "i386:x86-64",
            "second", "second.o"
        ],
        work_path
    )?;
    
    log::info!("Compiling macOS loader stub...");
    
    // Compile loader stub with osxcross
    run_command(
        gcc,
        &["-c", "loader_stub.c", "-o", "loader.o"],
        work_path
    )?;
    
    log::info!("Linking macOS Mach-O binary...");
    
    // Link everything with osxcross
    let output_name = "merged";
    run_command(
        gcc,
        &[
            "loader.o",
            "first.o",
            "second.o",
            "-o",
            output_name,
        ],
        work_path
    )?;
    
    let merged_path = work_path.join(output_name);
    
    log::info!("✅ macOS Mach-O binary merged successfully");
    
    Ok(merged_path.to_string_lossy().to_string())
}

/// Merge using native macOS tools (clang)
fn merge_macos_native(
    base_data: &[u8],
    overload_data: &[u8],
    mode: &str,
    sync: bool,
    work_path: &Path,
) -> Result<String> {
    // Similar to osxcross but using native tools
    let (first_data, second_data) = match mode {
        "before" => (overload_data, base_data),  // Overload runs first
        "after" => (base_data, overload_data),   // Base runs first
        _ => (overload_data, base_data),
    };
    
    log::info!("Merge mode: {}, Sync: {}", mode, sync);
    
    fs::write(work_path.join("first"), first_data)?;
    fs::write(work_path.join("second"), second_data)?;
    
    let loader_code = MACOS_LOADER_STUB.replace(
        "SYNC_MODE_PLACEHOLDER",
        if sync { "1" } else { "0" }
    );
    fs::write(work_path.join("loader_stub.c"), loader_code)?;
    
    // Use native objcopy (or ld -r)
    run_command("objcopy", &["-I", "binary", "-O", "mach-o-x86-64", "first", "first.o"], work_path)?;
    run_command("objcopy", &["-I", "binary", "-O", "mach-o-x86-64", "second", "second.o"], work_path)?;
    
    // Compile with clang
    run_command("clang", &["-c", "loader_stub.c", "-o", "loader.o"], work_path)?;
    run_command("clang", &["loader.o", "first.o", "second.o", "-o", "merged"], work_path)?;
    
    Ok(work_path.join("merged").to_string_lossy().to_string())
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
    #[ignore] // Requires osxcross or native macOS tools
    fn test_macos_macho_merge() {
        let temp_dir = TempDir::new().unwrap();
        
        // Mach-O magic numbers: 0xfeedface (32-bit), 0xfeedfacf (64-bit)
        let base_data = vec![0xcf, 0xfa, 0xed, 0xfe]; // 64-bit Mach-O magic (little-endian)
        let overload_data = vec![0xcf, 0xfa, 0xed, 0xfe];
        
        let result = merge_macos_macho(
            &base_data,
            &overload_data,
            "before",
            true,  // sync mode
            temp_dir.path(),
        );
        
        // Will fail without osxcross or native macOS tools
        if result.is_err() {
            let err = result.unwrap_err().to_string();
            println!("Expected error (no toolchain): {}", err);
            assert!(err.contains("macOS Mach-O binary merging requires"));
        } else {
            println!("Success: macOS toolchain available!");
        }
    }
}
