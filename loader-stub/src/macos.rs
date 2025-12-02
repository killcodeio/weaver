use std::ffi::CString;
use std::fs::{self, File};
use std::io::Write;
use std::mem;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;

use nix::fcntl::OFlag;
use nix::sys::mman::{mmap, shm_open, shm_unlink, MapFlags, ProtFlags};
use nix::sys::signal::{kill, Signal};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execv, fork, getpid, sleep, ForkResult, Pid};

use crate::common::{
    self, evaluate_health_status, force_kill_delay, health_check_interval, init_health_status,
    log_async_mode_started, log_base_completed_terminating_overload, log_base_exited,
    log_base_killed_by_signal, log_base_start_failed, log_fallback_kill, log_grace_period_exceeded,
    log_health_monitor_started, log_health_monitoring_enabled, log_heartbeat_lost,
    log_network_failure_threshold, log_overload_requested_kill, log_overload_start_failed,
    log_overload_terminated_abnormally, log_shm_create_failed, log_shm_map_failed,
    log_starting_base, log_sync_mode_waiting, log_verification_failed, log_verification_successful,
    overload_kill_wait_duration, should_enable_health_monitoring, signal_overload_to_kill,
    HealthCheckResult,
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
    let mut shm_name_str = String::new();

    if should_enable_health_monitoring(sync_mode, grace_period, network_failure_kill_count) {
        let pid = getpid();
        shm_name_str = format!("/overload_health_{}", pid);
        let shm_name_c = CString::new(shm_name_str.clone()).unwrap();

        match shm_open(
            shm_name_c.as_c_str(),
            OFlag::O_CREAT | OFlag::O_RDWR,
            Mode::from_bits_truncate(0o600),
        ) {
            Ok(fd) => {
                let _ = nix::unistd::ftruncate(&fd, mem::size_of::<HealthStatus>() as i64);

                unsafe {
                    let ptr = mmap(
                        None,
                        std::num::NonZeroUsize::new(mem::size_of::<HealthStatus>()).unwrap(),
                        ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                        MapFlags::MAP_SHARED,
                        &fd,
                        0,
                    );

                    match ptr {
                        Ok(p) => {
                            health_ptr = p.as_ptr() as *mut HealthStatus;
                            init_health_status(health_ptr);
                            std::env::set_var("KILLCODE_HEALTH_SHM", &shm_name_str);
                            log_health_monitoring_enabled(&shm_name_str);
                        }
                        Err(e) => log_shm_map_failed(e),
                    }
                }
            }
            Err(e) => log_shm_create_failed(e),
        }
    }

    // 2. Prepare binaries (Write to temp files)
    let temp_dir = std::env::temp_dir();
    let pid = getpid();
    let base_path = temp_dir.join(format!("base_{}", pid));
    let overload_path = temp_dir.join(format!("overload_{}", pid));

    eprintln!("[KillCode] Writing base binary ({} bytes) to: {}", base_data.len(), base_path.display());
    eprintln!("[KillCode] Writing overload binary ({} bytes) to: {}", overload_data.len(), overload_path.display());

    // Helper to write and make executable
    let write_binary = |path: &PathBuf, data: &[u8]| -> Result<(), std::io::Error> {
        let mut file = File::create(path)?;
        file.write_all(data)?;
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(0o755);
        file.set_permissions(perms)?;
        Ok(())
    };

    write_binary(&base_path, &base_data)?;
    write_binary(&overload_path, &overload_data)?;

    // Helper to execute binary
    // Returns: Ok(Pid) if child started
    let execute_binary = |path: &PathBuf, name: &str| -> Result<Pid, String> {
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => Ok(child),
            Ok(ForkResult::Child) => {
                let path_c = CString::new(path.to_str().unwrap()).unwrap();
                let name_c = CString::new(name).unwrap();
                let args = [name_c];
                let _ = execv(&path_c, &args);
                common::log_execv_failed();
                std::process::exit(1);
            }
            Err(e) => Err(format!("fork failed: {}", e)),
        }
    };

    // 3. Start Overload
    let overload_pid = match execute_binary(&overload_path, "overload") {
        Ok(pid) => {
            if sync_mode {
                log_sync_mode_waiting(pid);
                match waitpid(pid, None) {
                    Ok(WaitStatus::Exited(_, code)) => {
                        if code != 0 {
                            log_verification_failed(code);
                            let _ = fs::remove_file(&base_path);
                            let _ = fs::remove_file(&overload_path);
                            if !shm_name_str.is_empty() {
                                let _ = shm_unlink(shm_name_str.as_str());
                            }
                            return Err("Overload verification failed".into());
                        }
                        log_verification_successful();
                        let _ = fs::remove_file(&overload_path);
                        None
                    }
                    _ => {
                        log_overload_terminated_abnormally();
                        return Err("Overload terminated abnormally".into());
                    }
                }
            } else {
                log_async_mode_started(pid);
                Some(pid)
            }
        }
        Err(e) => {
            log_overload_start_failed(&e);
            return Err(e.into());
        }
    };

    // 4. Start Health Monitor Thread
    let monitor_handle = if !sync_mode
        && !health_ptr.is_null()
        && (grace_period > 0 || network_failure_kill_count > 0)
    {
        let base_pid_cell = Arc::new(AtomicI32::new(0));
        let base_pid_clone = base_pid_cell.clone();
        let health_ptr_addr = health_ptr as usize;

        Some((
            thread::spawn(move || {
                log_health_monitor_started();
                let health_ptr = health_ptr_addr as *mut HealthStatus;
                loop {
                    thread::sleep(health_check_interval());

                    let base_pid = base_pid_clone.load(Ordering::Relaxed);
                    if base_pid <= 0 {
                        continue;
                    }

                    if kill(Pid::from_raw(base_pid), None).is_err() {
                        break;
                    }

                    unsafe {
                        match evaluate_health_status(health_ptr, grace_period, network_failure_kill_count) {
                            HealthCheckResult::Ok => {}
                            HealthCheckResult::GracePeriodExceeded { time_since_success, grace_period } => {
                                log_grace_period_exceeded(time_since_success, grace_period);
                                kill_base(base_pid);
                                break;
                            }
                            HealthCheckResult::NetworkFailureThreshold { failures, threshold } => {
                                log_network_failure_threshold(failures, threshold);
                                signal_overload_to_kill(health_ptr);
                                thread::sleep(overload_kill_wait_duration());
                                log_fallback_kill();
                                kill_base(base_pid);
                                break;
                            }
                            HealthCheckResult::OverloadRequestedKill => {
                                log_overload_requested_kill();
                                kill_base(base_pid);
                                break;
                            }
                            HealthCheckResult::HeartbeatLost => {
                                log_heartbeat_lost();
                                kill_base(base_pid);
                                break;
                            }
                        }
                    }
                }
            }),
            base_pid_cell,
        ))
    } else {
        None
    };

    // 5. Start Base
    log_starting_base();
    let base_exit_code = match execute_binary(&base_path, "base") {
        Ok(child) => {
            if let Some((_, ref pid_cell)) = monitor_handle {
                pid_cell.store(child.as_raw(), Ordering::Relaxed);
            }

            let mut status_code = -1;
            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => status_code = code,
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    log_base_killed_by_signal(sig);
                    status_code = -1;
                }
                _ => {}
            }

            if let Some(ov_pid) = overload_pid {
                log_base_completed_terminating_overload(ov_pid);
                let _ = kill(ov_pid, Signal::SIGTERM);
                sleep(1);
                match waitpid(ov_pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => {
                        let _ = kill(ov_pid, Signal::SIGKILL);
                        let _ = waitpid(ov_pid, None);
                    }
                    _ => {}
                }
            }
            status_code
        }
        Err(e) => {
            log_base_start_failed(&e);
            1
        }
    };

    if let Some((handle, _)) = monitor_handle {
        let _ = handle.join();
    }

    let _ = fs::remove_file(&base_path);
    if !sync_mode {
        let _ = fs::remove_file(&overload_path);
    }
    if !shm_name_str.is_empty() {
        let _ = shm_unlink(shm_name_str.as_str());
    }

    log_base_exited(base_exit_code);
    std::process::exit(base_exit_code);
}

/// Kill base process with SIGTERM followed by SIGKILL
fn kill_base(base_pid: i32) {
    let _ = kill(Pid::from_raw(base_pid), Signal::SIGTERM);
    thread::sleep(force_kill_delay());
    let _ = kill(Pid::from_raw(base_pid), Signal::SIGKILL);
}
