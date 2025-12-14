use std::time::{SystemTime, UNIX_EPOCH};

use crate::{HealthStatus, HEALTH_CHECK_INTERVAL};

/// Get current Unix timestamp in seconds
pub fn current_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Initialize health status struct with default values
pub unsafe fn init_health_status(health_ptr: *mut HealthStatus) {
    (*health_ptr).last_success = current_time();
    (*health_ptr).is_alive = 1;
    (*health_ptr).consecutive_failures = 0;
    (*health_ptr).should_kill_base = 0;
    (*health_ptr).parent_requests_kill = 0;
    (*health_ptr).base_pid = 0;
}

/// Check if health monitoring should be enabled
pub fn should_enable_health_monitoring(sync_mode: bool, grace_period: u32, network_failure_kill_count: u32) -> bool {
    !sync_mode && (grace_period > 0 || network_failure_kill_count > 0)
}

/// Result of health check evaluation
pub enum HealthCheckResult {
    /// Everything is fine, continue monitoring
    Ok,
    /// Grace period exceeded, kill base
    GracePeriodExceeded { time_since_success: i64, grace_period: u32 },
    /// Network failure threshold exceeded, signal overload to kill
    NetworkFailureThreshold { failures: i32, threshold: u32 },
    /// Overload requested base termination
    OverloadRequestedKill,
    /// Overload heartbeat lost
    HeartbeatLost,
}

/// Evaluate health status and determine if action is needed
/// Returns None if base process has exited, otherwise returns HealthCheckResult
pub unsafe fn evaluate_health_status(
    health_ptr: *const HealthStatus,
    grace_period: u32,
    network_failure_kill_count: u32,
) -> HealthCheckResult {
    let status = &*health_ptr;
    let now = current_time();
    let time_since_success = now - status.last_success;

    // Check 1: Grace period
    if grace_period > 0 && time_since_success > grace_period as i64 {
        return HealthCheckResult::GracePeriodExceeded {
            time_since_success,
            grace_period,
        };
    }

    // Check 2: Network failure threshold
    if network_failure_kill_count > 0 && status.consecutive_failures >= network_failure_kill_count as i32 {
        return HealthCheckResult::NetworkFailureThreshold {
            failures: status.consecutive_failures,
            threshold: network_failure_kill_count,
        };
    }

    // Check 3: Overload requested base kill
    if status.should_kill_base != 0 {
        return HealthCheckResult::OverloadRequestedKill;
    }

    // Check 4: Overload heartbeat lost
    if status.is_alive == 0 {
        return HealthCheckResult::HeartbeatLost;
    }

    HealthCheckResult::Ok
}

/// Signal overload to execute kill method by setting parent_requests_kill flag
pub unsafe fn signal_overload_to_kill(health_ptr: *mut HealthStatus) {
    (*health_ptr).parent_requests_kill = 1;
}

/// Get the health check interval as a Duration
pub fn health_check_interval() -> std::time::Duration {
    std::time::Duration::from_secs(HEALTH_CHECK_INTERVAL as u64)
}

/// Duration to wait for overload to execute kill method before fallback
pub fn overload_kill_wait_duration() -> std::time::Duration {
    std::time::Duration::from_secs(15)
}

// Log message helpers - centralized logging for consistent output

pub fn log_health_monitoring_enabled(shm_name: &str) {
    eprintln!("[KillCode] Health monitoring enabled: {}", shm_name);
}

pub fn log_health_monitor_started() {
    eprintln!("[KillCode] Health monitor started");
}

pub fn log_sync_mode_waiting(pid: impl std::fmt::Display) {
    eprintln!("[KillCode] Sync mode: Waiting for overload verification (PID: {})...", pid);
}

pub fn log_verification_failed(exit_code: impl std::fmt::Display) {
    eprintln!("[KillCode] ❌ Overload verification failed (exit code: {})", exit_code);
}

pub fn log_verification_successful() {
    eprintln!("[KillCode] ✅ Overload verification successful");
}

pub fn log_async_mode_started(pid: impl std::fmt::Display) {
    eprintln!("[KillCode] Async mode: Overload running in background (PID: {})", pid);
}

pub fn log_overload_start_failed(error: &str) {
    eprintln!("[KillCode] Failed to start overload binary: {}", error);
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn log_base_start_failed(error: &str) {
    eprintln!("[KillCode] Failed to start base binary: {}", error);
}

pub fn log_starting_base() {
    eprintln!("[KillCode] Starting base binary...");
}

pub fn log_base_completed_terminating_overload(pid: impl std::fmt::Display) {
    eprintln!("[KillCode] Base binary completed, terminating overload (PID: {})", pid);
}

pub fn log_base_exited(exit_code: impl std::fmt::Display) {
    eprintln!("[KillCode] Base binary exited with code: {}", exit_code);
}

pub fn log_grace_period_exceeded(time_since_success: i64, grace_period: u32) {
    eprintln!("[KillCode] ⚠️  Grace period exceeded ({} > {} seconds), killing base", time_since_success, grace_period);
}

pub fn log_network_failure_threshold(failures: i32, threshold: u32) {
    eprintln!("[KillCode] ⚠️  Network failure threshold exceeded ({}/{}), signaling overload to kill parent", failures, threshold);
}

pub fn log_fallback_kill() {
    eprintln!("[KillCode] Fallback: Killing base directly (overload didn't respond)");
}

pub fn log_overload_requested_kill() {
    eprintln!("[KillCode] ⚠️  Overload requested base termination");
}

pub fn log_heartbeat_lost() {
    eprintln!("[KillCode] ⚠️  Overload heartbeat lost, killing base");
}

#[cfg(target_os = "linux")]
pub fn log_forcing_sigkill() {
    eprintln!("[KillCode] Forcing SIGKILL on overload");
}

pub fn log_shm_map_failed(error: impl std::fmt::Display) {
    eprintln!("[KillCode] Warning: Failed to map shared memory: {}", error);
}

pub fn log_shm_create_failed(error: impl std::fmt::Display) {
    eprintln!("[KillCode] Warning: Failed to create shared memory: {}", error);
}

#[cfg(target_os = "macos")]
pub fn log_overload_terminated_abnormally() {
    eprintln!("[KillCode] ❌ Overload terminated abnormally");
}

#[cfg(unix)]
pub fn log_execv_failed() {
    eprintln!("[KillCode] execv failed");
}

#[cfg(unix)]
pub fn log_base_killed_by_signal(signal: impl std::fmt::Display) {
    eprintln!("[KillCode] Base process killed by signal: {}", signal);
}

/// Short delay used when force-killing processes (unix only)
#[cfg(unix)]
pub fn force_kill_delay() -> std::time::Duration {
    std::time::Duration::from_millis(100)
}
