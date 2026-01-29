// Automatic system detection and configuration for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;

use crate::helpers::get_fstype;
use crate::meminfo::get_ram_size;
use crate::{debug, info};

/// Constants for size calculations
const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;

/// Storage type detection
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StorageType {
    NVMe,
    SSD,
    HDD,
    EMMC,
    SD,
    Tmpfs,      // LiveCD, RAM disk
    Unknown,
}

impl StorageType {
    /// Detect storage type for a path
    pub fn detect(path: &str) -> Self {
        // 1. Check if tmpfs (LiveCD, RAM)
        if let Some(fstype) = get_fstype(path) {
            if fstype == "tmpfs" || fstype == "squashfs" || fstype == "overlay" {
                return StorageType::Tmpfs;
            }
        }

        // 2. Find the underlying block device
        let device = match Self::find_block_device(path) {
            Some(d) => d,
            None => return StorageType::Unknown,
        };

        // 3. Check device type via /sys/block/
        let device_name = device.trim_start_matches("/dev/");
        let base_device = Self::get_base_device(device_name);

        // NVMe detection
        if base_device.starts_with("nvme") {
            return StorageType::NVMe;
        }

        // Check rotational flag
        let rotational_path = format!("/sys/block/{}/queue/rotational", base_device);
        if let Ok(content) = std::fs::read_to_string(&rotational_path) {
            match content.trim() {
                "0" => {
                    // Non-rotational: SSD, eMMC, or SD
                    if base_device.starts_with("mmcblk") {
                        // Check if eMMC or SD
                        let uevent_path = format!("/sys/block/{}/device/uevent", base_device);
                        if let Ok(uevent) = std::fs::read_to_string(&uevent_path) {
                            if uevent.contains("MMC_TYPE=MMC") {
                                return StorageType::EMMC;
                            } else if uevent.contains("MMC_TYPE=SD") {
                                return StorageType::SD;
                            }
                        }
                        return StorageType::EMMC;  // Default to eMMC for mmcblk
                    }
                    return StorageType::SSD;
                }
                "1" => return StorageType::HDD,
                _ => {}
            }
        }

        StorageType::Unknown
    }

    /// Find block device for a path using /proc/mounts
    fn find_block_device(path: &str) -> Option<String> {
        use std::process::{Command, Stdio};
        
        // First try findmnt
        let output = Command::new("findmnt")
            .args(["-n", "-o", "SOURCE", "--target", path])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;

        let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !source.is_empty() && source.starts_with('/') {
            return Some(source);
        }

        // Fallback: check root device
        let output = Command::new("findmnt")
            .args(["-n", "-o", "SOURCE", "--target", "/"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()?;

        let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !source.is_empty() && source.starts_with('/') {
            return Some(source);
        }

        None
    }

    /// Get base device name (nvme0n1p1 -> nvme0n1, sda1 -> sda)
    fn get_base_device(device: &str) -> String {
        // Handle NVMe: nvme0n1p1 -> nvme0n1
        if device.starts_with("nvme") {
            if let Some(pos) = device.find("p") {
                // Check if there's a digit after 'p' (partition number)
                let after_p = &device[pos + 1..];
                if after_p.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    return device[..pos].to_string();
                }
            }
            return device.to_string();
        }

        // Handle mmcblk: mmcblk0p1 -> mmcblk0
        if device.starts_with("mmcblk") {
            if let Some(pos) = device.find("p") {
                let after_p = &device[pos + 1..];
                if after_p.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    return device[..pos].to_string();
                }
            }
            return device.to_string();
        }

        // Handle regular: sda1 -> sda, vda2 -> vda
        let base: String = device.chars().take_while(|c| !c.is_ascii_digit()).collect();
        if base.is_empty() {
            device.to_string()
        } else {
            base
        }
    }
}

/// RAM profile based on total memory
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RamProfile {
    UltraLow,   // <= 2GB
    Low,        // 2-4GB
    Medium,     // 4-8GB
    Standard,   // 8-16GB
    High,       // 16-32GB
    VeryHigh,   // > 32GB
}

impl RamProfile {
    pub fn detect() -> Self {
        let ram = get_ram_size().unwrap_or(0);

        match ram {
            r if r <= 2 * GB => RamProfile::UltraLow,
            r if r <= 4 * GB => RamProfile::Low,
            r if r <= 8 * GB => RamProfile::Medium,
            r if r <= 16 * GB => RamProfile::Standard,
            r if r <= 32 * GB => RamProfile::High,
            _ => RamProfile::VeryHigh,
        }
    }

    /// Recommended zram algorithm based on RAM
    /// Low RAM: zstd for max compression
    /// High RAM: lz4 for max speed
    pub fn recommended_zram_alg(&self) -> &'static str {
        match self {
            RamProfile::UltraLow | RamProfile::Low => "zstd",
            _ => "lz4",
        }
    }

    /// Recommended zram size percentage
    /// All profiles: 80-100% (users with more RAM often demand more)
    pub fn recommended_zram_size_percent(&self) -> u32 {
        match self {
            RamProfile::UltraLow | RamProfile::Low => 100,
            RamProfile::Medium => 100,
            _ => 80,  // High RAM users still demand more
        }
    }

    /// Recommended zram mem_limit percentage (real RAM protection)
    pub fn recommended_zram_mem_limit_percent(&self) -> u32 {
        match self {
            RamProfile::UltraLow => 40,   // Tight, need working RAM
            RamProfile::Low => 50,
            RamProfile::Medium => 60,
            RamProfile::Standard => 70,
            RamProfile::High | RamProfile::VeryHigh => 75,
        }
    }

    /// Recommended MGLRU min_ttl_ms for working set protection
    pub fn recommended_mglru_min_ttl(&self) -> u32 {
        match self {
            RamProfile::UltraLow => 5000,   // 5s - maximum protection
            RamProfile::Low => 3000,        // 3s
            RamProfile::Medium => 2000,     // 2s
            RamProfile::Standard => 1000,   // 1s
            RamProfile::High => 500,        // 0.5s
            RamProfile::VeryHigh => 250,    // 0.25s - RAM is abundant
        }
    }

    /// Recommended zswap compressor
    pub fn recommended_zswap_compressor(&self) -> &'static str {
        match self {
            RamProfile::UltraLow | RamProfile::Low => "zstd",
            _ => "lz4",
        }
    }
}

/// Full system capabilities
#[derive(Debug, Clone)]
pub struct SystemCapabilities {
    pub ram_profile: RamProfile,
    pub storage_type: StorageType,
    pub swap_path_fstype: Option<String>,
    pub free_disk_space_bytes: u64,
    pub total_ram_bytes: u64,
    pub is_live_system: bool,
}

impl SystemCapabilities {
    /// Detect system capabilities
    pub fn detect() -> Self {
        let swap_path = "/swapfc";
        let swap_path_fstype = get_fstype(swap_path).or_else(|| get_fstype("/"));
        let storage_type = StorageType::detect(swap_path);
        let ram_profile = RamProfile::detect();
        let total_ram = get_ram_size().unwrap_or(0);
        let free_space = Self::get_free_disk_space(swap_path).unwrap_or(0);

        let is_live = matches!(storage_type, StorageType::Tmpfs)
            || swap_path_fstype.as_deref() == Some("squashfs")
            || swap_path_fstype.as_deref() == Some("overlay");

        if is_live {
            info!("Autoconfig: Detected LiveCD/Live system - will use zram only");
        }

        info!("Autoconfig: RAM={:?} ({} MB), Storage={:?}, FS={:?}", 
            ram_profile, 
            total_ram / MB,
            storage_type, 
            swap_path_fstype);

        Self {
            ram_profile,
            storage_type,
            swap_path_fstype,
            free_disk_space_bytes: free_space,
            total_ram_bytes: total_ram,
            is_live_system: is_live,
        }
    }

    /// Get free disk space for a path using statvfs
    fn get_free_disk_space(path: &str) -> Option<u64> {
        let check_path = if Path::new(path).exists() {
            path.to_string()
        } else {
            "/".to_string()
        };

        nix::sys::statvfs::statvfs(check_path.as_str())
            .ok()
            .map(|stat| stat.blocks_available() * stat.block_size())
    }
}

/// Swap mode recommendation
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SwapMode {
    ZramOnly,       // zram without disk backing
    ZramSwapfc,     // zram with swapfc overflow
    ZswapSwapfc,    // zswap with swap files
}

/// Recommended swap configuration
#[derive(Debug, Clone)]
pub struct RecommendedConfig {
    pub swap_mode: SwapMode,
    pub use_zswap: bool,
    pub use_swapfc: bool,
    
    // Zram settings
    pub zram_enabled: bool,
    pub zram_size_percent: u32,
    pub zram_algorithm: String,
    pub zram_mem_limit_percent: u32,
    
    // Zswap settings
    pub zswap_enabled: bool,
    pub zswap_compressor: String,
    pub zswap_max_pool_percent: u32,
    pub zswap_zpool: String,
    
    // Swapfc settings
    pub swapfc_enabled: bool,
    pub swapfc_use_sparse: bool,
    pub swapfc_directio: bool,
    pub swapfc_chunk_size: String,
    
    // MGLRU settings
    pub mglru_min_ttl_ms: u32,
}

impl Default for RecommendedConfig {
    fn default() -> Self {
        Self {
            swap_mode: SwapMode::ZramOnly,
            use_zswap: false,
            use_swapfc: false,
            zram_enabled: true,
            zram_size_percent: 80,
            zram_algorithm: "lz4".to_string(),
            zram_mem_limit_percent: 70,
            zswap_enabled: false,
            zswap_compressor: "lz4".to_string(),
            zswap_max_pool_percent: 25,
            zswap_zpool: "z3fold".to_string(),
            swapfc_enabled: false,
            swapfc_use_sparse: false,
            swapfc_directio: false,
            swapfc_chunk_size: "256M".to_string(),
            mglru_min_ttl_ms: 1000,
        }
    }
}

impl RecommendedConfig {
    /// Generate recommended configuration based on system capabilities
    pub fn from_capabilities(caps: &SystemCapabilities) -> Self {
        let ram = &caps.ram_profile;
        
        // LiveCD or tmpfs: zram only
        if caps.is_live_system {
            debug!("Autoconfig: Live system detected, using zram only");
            return Self::for_live_system(ram);
        }

        // HDD: prefer zram to avoid thrashing
        if matches!(caps.storage_type, StorageType::HDD) {
            info!("Autoconfig: HDD detected - using zram only to avoid thrashing");
            return Self::for_hdd(ram);
        }

        // eMMC/SD: protect wear, minimize disk writes
        if matches!(caps.storage_type, StorageType::EMMC | StorageType::SD) {
            info!("Autoconfig: eMMC/SD detected - using zram only to protect wear");
            return Self::for_emmc(ram);
        }

        // Check if filesystem supports swap files
        let supports_swapfiles = caps.swap_path_fstype.as_deref()
            .map(|fs| matches!(fs, "btrfs" | "ext4" | "xfs"))
            .unwrap_or(false);

        // SSD/NVMe with supported filesystem and enough space: zswap + swapfc
        if supports_swapfiles && caps.free_disk_space_bytes > 4 * GB {
            let is_nvme = matches!(caps.storage_type, StorageType::NVMe);
            info!("Autoconfig: {} + {} - using zswap + swapfc", 
                if is_nvme { "NVMe" } else { "SSD" },
                caps.swap_path_fstype.as_deref().unwrap_or("unknown"));
            return Self::for_ssd(ram, is_nvme);
        }

        // Fallback: zram only
        info!("Autoconfig: Fallback to zram only");
        Self::for_fallback(ram)
    }

    fn for_live_system(ram: &RamProfile) -> Self {
        Self {
            swap_mode: SwapMode::ZramOnly,
            use_zswap: false,
            use_swapfc: false,
            zram_enabled: true,
            zram_size_percent: 100,  // Max for live systems
            zram_algorithm: ram.recommended_zram_alg().to_string(),
            zram_mem_limit_percent: 50,  // Protect RAM on live systems
            zswap_enabled: false,
            zswap_compressor: "zstd".to_string(),
            zswap_max_pool_percent: 0,
            zswap_zpool: "z3fold".to_string(),
            swapfc_enabled: false,
            swapfc_use_sparse: false,
            swapfc_directio: false,
            swapfc_chunk_size: "256M".to_string(),
            mglru_min_ttl_ms: ram.recommended_mglru_min_ttl(),
        }
    }

    fn for_hdd(ram: &RamProfile) -> Self {
        Self {
            swap_mode: SwapMode::ZramOnly,
            use_zswap: false,
            use_swapfc: false,
            zram_enabled: true,
            zram_size_percent: ram.recommended_zram_size_percent(),
            zram_algorithm: ram.recommended_zram_alg().to_string(),
            zram_mem_limit_percent: ram.recommended_zram_mem_limit_percent(),
            zswap_enabled: false,
            zswap_compressor: "zstd".to_string(),
            zswap_max_pool_percent: 0,
            zswap_zpool: "z3fold".to_string(),
            swapfc_enabled: false,
            swapfc_use_sparse: false,
            swapfc_directio: false,  // HDD: no direct I/O
            swapfc_chunk_size: "256M".to_string(),
            mglru_min_ttl_ms: ram.recommended_mglru_min_ttl(),
        }
    }

    fn for_emmc(ram: &RamProfile) -> Self {
        Self {
            swap_mode: SwapMode::ZramOnly,
            use_zswap: false,
            use_swapfc: false,
            zram_enabled: true,
            zram_size_percent: ram.recommended_zram_size_percent(),
            zram_algorithm: "zstd".to_string(),  // Max compression = less overflow
            zram_mem_limit_percent: ram.recommended_zram_mem_limit_percent(),
            zswap_enabled: false,
            zswap_compressor: "zstd".to_string(),
            zswap_max_pool_percent: 0,
            zswap_zpool: "z3fold".to_string(),
            swapfc_enabled: false,
            swapfc_use_sparse: false,
            swapfc_directio: false,
            swapfc_chunk_size: "256M".to_string(),
            mglru_min_ttl_ms: ram.recommended_mglru_min_ttl() * 2,  // Extra protection
        }
    }

    fn for_ssd(ram: &RamProfile, is_nvme: bool) -> Self {
        Self {
            swap_mode: SwapMode::ZswapSwapfc,
            use_zswap: true,
            use_swapfc: true,
            zram_enabled: false,
            zram_size_percent: 0,
            zram_algorithm: "lz4".to_string(),
            zram_mem_limit_percent: 0,
            zswap_enabled: true,
            zswap_compressor: ram.recommended_zswap_compressor().to_string(),
            zswap_max_pool_percent: 25,  // Uniform for all RAM profiles
            zswap_zpool: "z3fold".to_string(),
            swapfc_enabled: true,
            swapfc_use_sparse: true,     // Disk-efficient with zswap
            swapfc_directio: is_nvme,    // Direct I/O only on NVMe
            swapfc_chunk_size: if is_nvme { "512M" } else { "256M" }.to_string(),
            mglru_min_ttl_ms: ram.recommended_mglru_min_ttl(),
        }
    }

    fn for_fallback(ram: &RamProfile) -> Self {
        Self::for_live_system(ram)
    }
}
