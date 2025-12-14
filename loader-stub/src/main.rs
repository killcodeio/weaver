use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem;

mod common;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "macos")]
mod macos;

const MAGIC_BYTES: &[u8; 8] = b"KILLCODE";
const HEALTH_CHECK_INTERVAL: u32 = 5;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ConfigFooter {
    pub magic: [u8; 8],
    pub base_offset: u64,
    pub base_size: u64,
    pub overload_offset: u64,
    pub overload_size: u64,
    pub grace_period: u32,
    pub sync_mode: u8, // 0 or 1
    pub network_failure_kill_count: u32,
}

#[repr(C)]
pub struct HealthStatus {
    pub last_success: i64,          // Timestamp of last successful check (time_t)
    pub consecutive_failures: i32,  // Counter of network failures
    pub is_alive: i32,              // Heartbeat flag (1=alive, 0=dead)
    pub should_kill_base: i32,      // Signal from overload to kill base
    pub parent_requests_kill: i32,  // Signal from parent: kill yourself now
    pub base_pid: i32,              // PID of the base process
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Read self
    let mut self_file = File::open(std::env::current_exe()?)?;
    let file_len = self_file.metadata()?.len();

    if file_len < mem::size_of::<ConfigFooter>() as u64 {
        return Err("File too small to contain footer".into());
    }

    // 2. Read footer
    self_file.seek(SeekFrom::End(-(mem::size_of::<ConfigFooter>() as i64)))?;
    let mut footer_bytes = [0u8; mem::size_of::<ConfigFooter>()];
    self_file.read_exact(&mut footer_bytes)?;

    let footer: ConfigFooter = unsafe { mem::transmute(footer_bytes) };

    if &footer.magic != MAGIC_BYTES {
        return Err("Invalid magic bytes in footer".into());
    }

    eprintln!("[KillCode] V2 Stub execution starting");
    eprintln!("[KillCode] Config: sync={}, grace_period={}s, failure_threshold={}", 
             footer.sync_mode, footer.grace_period, footer.network_failure_kill_count);

    // 3. Read binaries
    let mut base_data = vec![0u8; footer.base_size as usize];
    self_file.seek(SeekFrom::Start(footer.base_offset))?;
    self_file.read_exact(&mut base_data)?;

    let mut overload_data = vec![0u8; footer.overload_size as usize];
    self_file.seek(SeekFrom::Start(footer.overload_offset))?;
    self_file.read_exact(&mut overload_data)?;

    // Dispatch to OS-specific implementation
    #[cfg(target_os = "linux")]
    return linux::run(base_data, overload_data, footer);

    #[cfg(target_os = "windows")]
    return windows::run(base_data, overload_data, footer);

    #[cfg(target_os = "macos")]
    return macos::run(base_data, overload_data, footer);

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    return Err("Unsupported platform".into());
}
