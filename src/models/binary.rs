use chrono::{DateTime, Utc};
use std::fmt;
use goblin::Object;

#[derive(Debug, Clone)]
pub struct StoredBinary {
    pub id: String,
    pub path: String,
    pub size: u64,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Platform {
    LINUX_ELF,
    WINDOWS_PE,
    MACOS_MACH_O,
    UNKNOWN,
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl Platform {
    pub fn detect(data: &[u8]) -> Self {
        match Object::parse(data) {
            Ok(Object::Elf(_)) => Platform::LINUX_ELF,
            Ok(Object::PE(_)) => Platform::WINDOWS_PE,
            Ok(Object::Mach(_)) => Platform::MACOS_MACH_O,
            _ => Platform::UNKNOWN,
        }
    }
    
    pub fn name(&self) -> &'static str {
        match self {
            Platform::LINUX_ELF => "Linux ELF",
            Platform::WINDOWS_PE => "Windows PE",
            Platform::MACOS_MACH_O => "macOS Mach-O",
            Platform::UNKNOWN => "Unknown",
        }
    }
    
    pub fn is_supported(&self) -> bool {
        matches!(self, Platform::LINUX_ELF)
    }
}
