use std::ffi::CString;
use std::fs;
use std::mem;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Environment::SetEnvironmentVariableA;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingA, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessA, GetCurrentProcessId, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
    INFINITE, PROCESS_INFORMATION, STARTUPINFOA,
};

use crate::common::{
    evaluate_health_status, health_check_interval, init_health_status, log_async_mode_started,
    log_base_completed_terminating_overload, log_base_exited, log_base_start_failed,
    log_fallback_kill, log_grace_period_exceeded, log_health_monitor_started,
    log_health_monitoring_enabled, log_heartbeat_lost, log_network_failure_threshold,
    log_overload_requested_kill, log_overload_start_failed, log_shm_create_failed,
    log_shm_map_failed, log_starting_base, log_sync_mode_waiting, log_verification_failed,
    log_verification_successful, overload_kill_wait_duration, should_enable_health_monitoring,
    signal_overload_to_kill, HealthCheckResult,
};
use crate::{ConfigFooter, HealthStatus};

pub fn run(
    base_data: Vec<u8>,
    overload_data: Vec<u8>,
    footer: ConfigFooter,
) -> Result<(), Box<dyn std::error::Error>> {
    let sync_mode = footer.sync_mode != 0;
    let grace_period = footer.grace_period;
    let network_failure_kill_count = footer.network_failure_kill_count;

    // 1. Setup Shared Memory (if async and monitoring needed)
    let mut health_ptr: *mut HealthStatus = ptr::null_mut();
    let mut health_shm_handle: HANDLE = ptr::null_mut();
    let mut health_view: MEMORY_MAPPED_VIEW_ADDRESS = unsafe { mem::zeroed() };

    if should_enable_health_monitoring(sync_mode, grace_period, network_failure_kill_count) {
        unsafe {
            let pid = GetCurrentProcessId();
            let shm_name = format!("Local\\OverloadHealth_{}", pid);
            let shm_name_c = CString::new(shm_name.clone()).unwrap();

            health_shm_handle = CreateFileMappingA(
                INVALID_HANDLE_VALUE,
                ptr::null(),
                PAGE_READWRITE,
                0,
                mem::size_of::<HealthStatus>() as u32,
                shm_name_c.as_ptr() as *const u8,
            );

            if health_shm_handle != ptr::null_mut() {
                health_view = MapViewOfFile(
                    health_shm_handle,
                    FILE_MAP_ALL_ACCESS,
                    0,
                    0,
                    mem::size_of::<HealthStatus>(),
                );

                if !health_view.Value.is_null() {
                    health_ptr = health_view.Value as *mut HealthStatus;
                    init_health_status(health_ptr);

                    // Set env var for overload
                    let env_name = CString::new("KILLCODE_HEALTH_SHM").unwrap();
                    SetEnvironmentVariableA(
                        env_name.as_ptr() as *const u8,
                        shm_name_c.as_ptr() as *const u8,
                    );
                    log_health_monitoring_enabled(&shm_name);
                } else {
                    log_shm_map_failed("MapViewOfFile returned null");
                }
            } else {
                log_shm_create_failed(GetLastError());
            }
        }
    }

    // 2. Prepare binaries
    let temp_dir = std::env::temp_dir();
    let base_path = temp_dir.join("base.exe");
    let overload_path = temp_dir.join("overload.exe");

    // Write binaries
    fs::write(&base_path, &base_data)?;
    fs::write(&overload_path, &overload_data)?;

    // Helper to execute binary
    let execute_binary = |path: &PathBuf, is_base: bool| -> Result<(HANDLE, u32), String> {
        unsafe {
            let path_str = path.to_str().ok_or("Invalid path")?;
            let path_c = CString::new(path_str).map_err(|_| "Invalid path CString")?;
            
            let mut si: STARTUPINFOA = mem::zeroed();
            si.cb = mem::size_of::<STARTUPINFOA>() as u32;
            let mut pi: PROCESS_INFORMATION = mem::zeroed();

            // CreateProcessA requires a mutable command line string if the first arg is NULL,
            // OR if the first arg is provided, it uses that as the executable.
            // We'll pass the path as the first argument (lpApplicationName) and NULL for command line.
            let success = CreateProcessA(
                path_c.as_ptr() as *const u8,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                0,
                0,
                ptr::null(),
                ptr::null(),
                &si,
                &mut pi,
            );

            if success == 0 {
                return Err(format!("CreateProcessA failed: {}", GetLastError()));
            }

            CloseHandle(pi.hThread);
            Ok((pi.hProcess, pi.dwProcessId))
        }
    };

    // 3. Start Overload
    let mut overload_handle: HANDLE = ptr::null_mut();
    let mut overload_pid: u32 = 0;

    match execute_binary(&overload_path, false) {
        Ok((h, pid)) => {
            overload_handle = h;
            overload_pid = pid;

            if sync_mode {
                log_sync_mode_waiting(overload_pid);
                unsafe {
                    WaitForSingleObject(overload_handle, INFINITE);
                    let mut exit_code: u32 = 0;
                    GetExitCodeProcess(overload_handle, &mut exit_code);

                    if exit_code != 0 {
                        log_verification_failed(exit_code);
                        CloseHandle(overload_handle);
                        let _ = fs::remove_file(&base_path);
                        let _ = fs::remove_file(&overload_path);
                        return Err("Overload verification failed".into());
                    }
                    log_verification_successful();
                }
            } else {
                log_async_mode_started(overload_pid);
            }
        }
        Err(e) => {
            log_overload_start_failed(&e);
            let _ = fs::remove_file(&base_path);
            let _ = fs::remove_file(&overload_path);
            return Err(e.into());
        }
    }

    // 4. Start Base
    log_starting_base();
    let (base_handle, base_pid) = match execute_binary(&base_path, true) {
        Ok((h, pid)) => {
            if !health_ptr.is_null() {
                unsafe { (*health_ptr).base_pid = pid as i32; }
            }
            (h, pid)
        },
        Err(e) => {
            log_base_start_failed(&e);
            if overload_handle != ptr::null_mut() {
                unsafe {
                    TerminateProcess(overload_handle, 0);
                    CloseHandle(overload_handle);
                }
            }
            let _ = fs::remove_file(&base_path);
            let _ = fs::remove_file(&overload_path);
            return Err(e.into());
        }
    };

    // 5. Start Health Monitor Thread
    let monitor_running = Arc::new(AtomicBool::new(true));
    let monitor_handle = if !sync_mode
        && !health_ptr.is_null()
        && (grace_period > 0 || network_failure_kill_count > 0)
    {
        let monitor_running_clone = monitor_running.clone();
        let health_ptr_addr = health_ptr as usize;
        let base_handle_val = base_handle as usize;

        Some(thread::spawn(move || {
            log_health_monitor_started();
            let health_ptr = health_ptr_addr as *mut HealthStatus;
            let base_handle = base_handle_val as HANDLE;

            while monitor_running_clone.load(Ordering::Relaxed) {
                thread::sleep(health_check_interval());

                if !monitor_running_clone.load(Ordering::Relaxed) {
                    break;
                }

                unsafe {
                    // Check if base is still running
                    let mut exit_code: u32 = 0;
                    if GetExitCodeProcess(base_handle, &mut exit_code) != 0 && exit_code != 259 {
                        break; // Base finished (259 is STILL_ACTIVE)
                    }

                    match evaluate_health_status(health_ptr, grace_period, network_failure_kill_count) {
                        HealthCheckResult::Ok => {}
                        HealthCheckResult::GracePeriodExceeded { time_since_success, grace_period } => {
                            log_grace_period_exceeded(time_since_success, grace_period);
                            TerminateProcess(base_handle, 1);
                            break;
                        }
                        HealthCheckResult::NetworkFailureThreshold { failures, threshold } => {
                            log_network_failure_threshold(failures, threshold);
                            signal_overload_to_kill(health_ptr);
                            thread::sleep(overload_kill_wait_duration());
                            log_fallback_kill();
                            TerminateProcess(base_handle, 1);
                            break;
                        }
                        HealthCheckResult::OverloadRequestedKill => {
                            log_overload_requested_kill();
                            TerminateProcess(base_handle, 1);
                            break;
                        }
                        HealthCheckResult::HeartbeatLost => {
                            log_heartbeat_lost();
                            TerminateProcess(base_handle, 1);
                            break;
                        }
                    }
                }
            }
        }))
    } else {
        None
    };

    // 6. Wait for Base
    unsafe {
        WaitForSingleObject(base_handle, INFINITE);
        let mut base_exit_code: u32 = 0;
        GetExitCodeProcess(base_handle, &mut base_exit_code);
        
        // Stop monitor
        monitor_running.store(false, Ordering::Relaxed);
        if let Some(handle) = monitor_handle {
            let _ = handle.join();
        }

        // Cleanup Base
        CloseHandle(base_handle);
        // We can try to delete the file, but it might be locked for a moment.
        // Windows is picky about deleting running executables.
        // We'll try, but ignore errors.
        let _ = fs::remove_file(&base_path);

        // Cleanup Overload
        if overload_handle != ptr::null_mut() {
            log_base_completed_terminating_overload(overload_pid);
            TerminateProcess(overload_handle, 0);
            CloseHandle(overload_handle);
            let _ = fs::remove_file(&overload_path);
        }

        // Cleanup Shared Memory
        if !health_ptr.is_null() {
            UnmapViewOfFile(health_view);
        }
        if health_shm_handle != ptr::null_mut() {
            CloseHandle(health_shm_handle);
        }

        log_base_exited(base_exit_code);
        std::process::exit(base_exit_code as i32);
    }
}
