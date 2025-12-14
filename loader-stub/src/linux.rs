use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;

use nix::fcntl::OFlag;
use nix::sys::memfd::{memfd_create, MFdFlags};
use nix::sys::mman::{mmap, shm_open, MapFlags, ProtFlags};
use nix::sys::signal::{kill, Signal};
use nix::sys::stat::Mode;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execv, fork, getpid, sleep, ForkResult, Pid};

use crate::common::{
    self, evaluate_health_status, force_kill_delay, health_check_interval, init_health_status,
    log_async_mode_started, log_base_completed_terminating_overload, log_base_exited,
    log_base_killed_by_signal, log_fallback_kill, log_forcing_sigkill, log_grace_period_exceeded,
    log_health_monitor_started, log_health_monitoring_enabled, log_heartbeat_lost,
    log_network_failure_threshold, log_overload_requested_kill, log_overload_start_failed,
    log_shm_create_failed, log_shm_map_failed, log_starting_base, log_sync_mode_waiting,
    log_verification_failed, log_verification_successful, overload_kill_wait_duration,
    should_enable_health_monitoring, signal_overload_to_kill, HealthCheckResult,
};
use crate::{ConfigFooter, HealthStatus};

unsafe fn execute_binary(
    binary_data: &[u8],
    name: &str,
    is_base: bool,
    sync_mode: bool,
    overload_pid_ref: &mut Option<Pid>,
) -> Result<i32, String> {
    let name_c = CString::new(name).unwrap();
    let fd = memfd_create(name_c.as_c_str(), MFdFlags::MFD_CLOEXEC)
        .map_err(|e| format!("memfd_create failed: {}", e))?;

    let mut file = File::from(fd);
    file.write_all(binary_data)
        .map_err(|e| format!("Failed to write binary data: {}", e))?;

    let raw_fd = file.as_raw_fd();
    mem::forget(file);

    match fork() {
        Ok(ForkResult::Parent { child }) => {
            nix::unistd::close(raw_fd).ok();

            if !is_base {
                *overload_pid_ref = Some(child);

                if sync_mode {
                    log_sync_mode_waiting(child);
                    match waitpid(child, None) {
                        Ok(WaitStatus::Exited(_, code)) => {
                            if code != 0 {
                                log_verification_failed(code);
                                return Err(format!("Overload verification failed with code {}", code));
                            }
                            log_verification_successful();
                        }
                        Ok(status) => {
                            eprintln!("[KillCode] ❌ Overload terminated abnormally: {:?}", status);
                            return Err(format!("Overload terminated abnormally: {:?}", status));
                        }
                        Err(e) => return Err(format!("waitpid failed: {}", e)),
                    }
                } else {
                    log_async_mode_started(child);
                }
                Ok(0)
            } else {
                let mut status_code = -1;
                match waitpid(child, None) {
                    Ok(WaitStatus::Exited(_, code)) => status_code = code,
                    Ok(WaitStatus::Signaled(_, sig, _)) => {
                        log_base_killed_by_signal(sig);
                        status_code = -1;
                    }
                    Err(e) => eprintln!("[KillCode] waitpid failed for base: {}", e),
                    _ => {}
                }

                if let Some(overload_pid) = *overload_pid_ref {
                    log_base_completed_terminating_overload(overload_pid);
                    let _ = kill(overload_pid, Signal::SIGTERM);
                    sleep(1);

                    match waitpid(overload_pid, Some(WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::StillAlive) => {
                            log_forcing_sigkill();
                            let _ = kill(overload_pid, Signal::SIGKILL);
                            let _ = waitpid(overload_pid, None);
                        }
                        _ => {}
                    }
                }

                Ok(status_code)
            }
        }
        Ok(ForkResult::Child) => {
            let fd_path = format!("/proc/self/fd/{}", raw_fd);
            let fd_path_c = CString::new(fd_path).unwrap();
            let args = [name_c.clone()];
            let _ = execv(&fd_path_c, &args);
            common::log_execv_failed();
            std::process::exit(1);
        }
        Err(e) => {
            nix::unistd::close(raw_fd).ok();
            Err(format!("fork failed: {}", e))
        }
    }
}

/// Kill base process with SIGTERM followed by SIGKILL
fn kill_base(base_pid: i32) {
    let _ = kill(Pid::from_raw(base_pid), Signal::SIGTERM);
    thread::sleep(force_kill_delay());
    let _ = kill(Pid::from_raw(base_pid), Signal::SIGKILL);
}

pub fn run(
    base_data: Vec<u8>,
    overload_data: Vec<u8>,
    footer: ConfigFooter,
) -> Result<(), Box<dyn std::error::Error>> {
    let sync_mode = footer.sync_mode != 0;
    let grace_period = footer.grace_period;
    let network_failure_kill_count = footer.network_failure_kill_count;

    let mut health_ptr: *mut HealthStatus = ptr::null_mut();
    let mut _shm_fd_keeper = None;

    if should_enable_health_monitoring(sync_mode, grace_period, network_failure_kill_count) {
        let pid = getpid();
        let shm_name = format!("/overload_health_{}", pid);
        let shm_name_c = CString::new(shm_name.clone()).unwrap();

        match shm_open(
            shm_name_c.as_c_str(),
            OFlag::O_CREAT | OFlag::O_RDWR,
            Mode::from_bits_truncate(0o600),
        ) {
            Ok(fd) => {
                let _ = nix::unistd::ftruncate(&fd, mem::size_of::<HealthStatus>() as libc::off_t);

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
                            std::env::set_var("KILLCODE_HEALTH_SHM", &shm_name);
                            log_health_monitoring_enabled(&shm_name);
                            _shm_fd_keeper = Some(fd);
                        }
                        Err(e) => log_shm_map_failed(e),
                    }
                }
            }
            Err(e) => log_shm_create_failed(e),
        }
    }

    let mut overload_pid = None;
    unsafe {
        if let Err(e) = execute_binary(&overload_data, "overload", false, sync_mode, &mut overload_pid) {
            log_overload_start_failed(&e);
            return Err(e.into());
        }
    }

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

    log_starting_base();
    let base_exit_code = unsafe {
        let name_c = CString::new("base").unwrap();
        let fd = memfd_create(name_c.as_c_str(), MFdFlags::MFD_CLOEXEC)
            .map_err(|e| format!("memfd_create failed: {}", e))?;

        let mut file = File::from(fd);
        file.write_all(&base_data)
            .map_err(|e| format!("Failed to write binary data: {}", e))?;
        let raw_fd = file.as_raw_fd();
        mem::forget(file);

        match fork() {
            Ok(ForkResult::Parent { child }) => {
                nix::unistd::close(raw_fd).ok();

                if let Some((_, ref pid_cell)) = monitor_handle {
                    pid_cell.store(child.as_raw(), Ordering::Relaxed);
                }

                if !health_ptr.is_null() {
                    (*health_ptr).base_pid = child.as_raw();
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
                Ok(status_code)
            }
            Ok(ForkResult::Child) => {
                let fd_path = format!("/proc/self/fd/{}", raw_fd);
                let fd_path_c = CString::new(fd_path).unwrap();
                let args = [name_c.clone()];
                let _ = execv(&fd_path_c, &args);
                std::process::exit(1);
            }
            Err(e) => {
                nix::unistd::close(raw_fd).ok();
                Err(format!("fork failed: {}", e))
            }
        }
    }?;

    if let Some((handle, _)) = monitor_handle {
        let _ = handle.join();
    }

    log_base_exited(base_exit_code);
    std::process::exit(base_exit_code);
}
