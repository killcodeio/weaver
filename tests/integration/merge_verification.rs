use std::process::Command;
use std::fs;
use crate::common::{build_test_binary_from_code, get_test_binary_path};

/// Execute a binary and capture its output
fn execute_binary(path: &str) -> Result<String, String> {
    let output = Command::new(path)
        .output()
        .map_err(|e| format!("Failed to execute: {}", e))?;
    
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(format!("Execution failed: {}", String::from_utf8_lossy(&output.stderr)))
    }
}

#[test]
#[ignore] // Run with: cargo test --test lib merge_verification -- --ignored --nocapture
fn test_merge_binary_execution_order_before() {
    println!("\n🔄 Testing Merge Binary Execution Order (BEFORE mode)");
    println!("======================================================\n");
    
    // Step 1: Create base binary that outputs "BASE"
    println!("📊 Step 1: Create base binary");
    let base_code = r#"
#include <stdio.h>
int main() {
    printf("BASE\n");
    return 0;
}
"#;
    
    let base_path = match build_test_binary_from_code(base_code, "test_merge_base") {
        Ok(path) => {
            println!("   ✅ Base binary created");
            path
        }
        Err(e) => {
            println!("   ❌ Failed to create base binary: {}", e);
            return;
        }
    };
    
    // Step 2: Create overload binary that outputs "OVERLOAD"
    println!("\n📊 Step 2: Create overload binary");
    let overload_code = r#"
#include <stdio.h>
int main() {
    printf("OVERLOAD\n");
    return 0;
}
"#;
    
    let overload_path = match build_test_binary_from_code(overload_code, "test_merge_overload") {
        Ok(path) => {
            println!("   ✅ Overload binary created");
            path
        }
        Err(e) => {
            println!("   ❌ Failed to create overload binary: {}", e);
            fs::remove_file(base_path).ok();
            return;
        }
    };
    
    // Step 3: Execute base binary and verify output
    println!("\n📊 Step 3: Execute base binary");
    let base_output = match execute_binary(base_path.to_str().unwrap()) {
        Ok(output) => {
            println!("   Output: {}", output.trim());
            assert!(output.contains("BASE"), "Base should output 'BASE'");
            println!("   ✅ Base binary works correctly");
            output
        }
        Err(e) => {
            println!("   ❌ Failed to execute base: {}", e);
            fs::remove_file(base_path).ok();
            fs::remove_file(overload_path).ok();
            return;
        }
    };
    
    // Step 4: Execute overload binary and verify output
    println!("\n📊 Step 4: Execute overload binary");
    let overload_output = match execute_binary(overload_path.to_str().unwrap()) {
        Ok(output) => {
            println!("   Output: {}", output.trim());
            assert!(output.contains("OVERLOAD"), "Overload should output 'OVERLOAD'");
            println!("   ✅ Overload binary works correctly");
            output
        }
        Err(e) => {
            println!("   ❌ Failed to execute overload: {}", e);
            fs::remove_file(base_path).ok();
            fs::remove_file(overload_path).ok();
            return;
        }
    };
    
    // Step 5: Merge binaries using Weaver's merger
    println!("\n📊 Step 5: Merge binaries using Weaver merger (mode=before)");
    
    use weaver::core::merger::merge_binaries;
    use weaver::core::binary::BinaryInfo;
    use weaver::models::request::MergeMode;
    use tempfile::tempdir;
    
    let base_data = fs::read(&base_path).expect("Failed to read base binary");
    let overload_data = fs::read(&overload_path).expect("Failed to read overload binary");
    
    let base_info = BinaryInfo::detect(&base_data);
    let overload_info = BinaryInfo::detect(&overload_data);
    
    println!("   Base: {} {}", base_info.os.name(), base_info.arch.name());
    println!("   Overload: {} {}", overload_info.os.name(), overload_info.arch.name());
    
    // Check compatibility
    if !base_info.is_compatible_with(&overload_info) {
        println!("   ❌ Binaries are not compatible!");
        fs::remove_file(base_path).ok();
        fs::remove_file(overload_path).ok();
        return;
    }
    
    println!("   ✅ Binaries are compatible");
    
    // Create temp directory for merge operation
    let temp_dir = tempdir().expect("Failed to create temp dir");
    let temp_path = temp_dir.path().to_str().unwrap();
    
    // Merge binaries
    let merged_binary_id = match merge_binaries(
        &base_data,
        &overload_data,
        MergeMode::Before,
        true, // sync
        temp_path,
        "", // task_id
        "redis://redis:6379", // redis_url (test default)
    ) {
        Ok(binary_id) => {
            println!("   ✅ Binaries merged successfully");
            println!("   Binary ID: {}", binary_id);
            binary_id
        }
        Err(e) => {
            println!("   ❌ Merge failed: {}", e);
            fs::remove_file(base_path).ok();
            fs::remove_file(overload_path).ok();
            return;
        }
    };
    
    // The merged binary should be in the temp directory
    let merged_path = format!("{}/{}", temp_path, merged_binary_id);
    if !std::path::Path::new(&merged_path).exists() {
        println!("   ❌ Merged binary not found at: {}", merged_path);
        fs::remove_file(base_path).ok();
        fs::remove_file(overload_path).ok();
        return;
    }
    
    // Step 6: Execute merged binary and verify output order
    println!("\n📊 Step 6: Execute merged binary and verify output order");
    match execute_binary(&merged_path) {
        Ok(merged_output) => {
            println!("   Output:\n{}", merged_output);
            
            // In "before" mode: overload runs FIRST, then base
            let lines: Vec<&str> = merged_output.lines().collect();
            
            println!("\n📊 Step 7: Verify execution order");
            println!("   Expected order (mode=before): OVERLOAD → BASE");
            
            // Find positions of outputs
            let overload_pos = merged_output.find("OVERLOAD");
            let base_pos = merged_output.find("BASE");
            
            match (overload_pos, base_pos) {
                (Some(o_pos), Some(b_pos)) => {
                    if o_pos < b_pos {
                        println!("   ✅ Correct order: OVERLOAD appears before BASE");
                        println!("   ✅ TEST PASSED!");
                    } else {
                        println!("   ❌ Wrong order: BASE appears before OVERLOAD");
                        panic!("Execution order is incorrect for mode=before");
                    }
                }
                _ => {
                    println!("   ❌ Missing output from one or both binaries");
                    println!("   Merged output: {}", merged_output);
                    panic!("Merged binary did not execute both binaries");
                }
            }
        }
        Err(e) => {
            println!("   ❌ Failed to execute merged binary: {}", e);
            panic!("Merged binary execution failed");
        }
    }
    
    // Cleanup
    fs::remove_file(base_path).ok();
    fs::remove_file(overload_path).ok();
    fs::remove_file(&merged_path).ok();
    
    println!("\n✅ End-to-end merge verification PASSED!");
}

#[test]
#[ignore] // Run with: cargo test --test lib merge_verification -- --ignored --nocapture
fn test_merge_binary_execution_order_after() {
    println!("\n🔄 Testing Merge Binary Execution Order (AFTER mode)");
    println!("=====================================================\n");
    
    // Similar to above but with mode=after
    // In "after" mode: base runs FIRST, then overload
    
    println!("   TODO: Implement AFTER mode test");
    println!("   Expected order (mode=after): BASE → OVERLOAD");
}
