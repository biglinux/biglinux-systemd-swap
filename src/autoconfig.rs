// Automatic system detection and configuration for systemd-swap
// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::Path;
use std::process::{Command, Stdio};

use crate::helpers::get_fstype;
use crate::meminfo::get_ram_size;
use crate::{debug, info, warn};

/// Constants for size calculations
const KB: u64 = 1024;
const MB: u64 = 1024 * KB;
const GB: u64 = 1024 * MB;

/// Type of virtualization detected
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VirtualizationType {
    None,        // Bare metal
    KVM,         // KVM/QEMU (includes Proxmox, libvirt)
    VMware,
    VirtualBox,
    HyperV,
    Xen,
    WSL,         // Windows Subsystem for Linux
    Docker,      // Container (not VM but relevant)
    LXC,         // LXC Container
    Unknown,     // Some virtualization not identified
}

impl VirtualizationType {
    /// Detect if running in a virtualized environment
    pub fn detect() -> Self {
        // 1. Use systemd-detect-virt (most reliable)
        if let Ok(output) = Command::new("systemd-detect-virt")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            if output.status.success() {
                let virt = String::from_utf8_lossy(&output.stdout).trim().to_string();
                match virt.as_str() {
                    "none" => return VirtualizationType::None,
                    "kvm" | "qemu" => return VirtualizationType::KVM,
                    "vmware" => return VirtualizationType::VMware,
                    "oracle" => return VirtualizationType::VirtualBox,
                    "microsoft" => return VirtualizationType::HyperV,
                    "xen" | "xen-hvm" | "xen-pv" => return VirtualizationType::Xen,
                    "wsl" => return VirtualizationType::WSL,
                    "docker" => return VirtualizationType::Docker,
                    "lxc" | "lxc-libvirt" => return VirtualizationType::LXC,
                    _ => {}
                }
            }
        }
        
        // 2. Fallback: check /sys/class/dmi/id/product_name
        if let Ok(product) = std::fs::read_to_string("/sys/class/dmi/id/product_name") {
            let product = product.trim().to_lowercase();
            if product.contains("virtualbox") {
                return VirtualizationType::VirtualBox;
            }
            if product.contains("vmware") {
                return VirtualizationType::VMware;
            }
            if product.contains("kvm") || product.contains("qemu") {
                return VirtualizationType::KVM;
            }
            if product.contains("virtual machine") {
                return VirtualizationType::HyperV;
            }
        }
        
        // 3. Check /proc/cpuinfo for hypervisor flag
        if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
            if cpuinfo.contains("hypervisor") {
                return VirtualizationType::Unknown; // VM but unknown type
            }
        }
        
        VirtualizationType::None
    }
    
    /// Returns true if it's a container environment
    pub fn is_container(&self) -> bool {
        matches!(self, VirtualizationType::Docker | VirtualizationType::LXC)
    }
    
    /// Returns true if it's a traditional VM
    pub fn is_vm(&self) -> bool {
        matches!(self, 
            VirtualizationType::KVM | 
            VirtualizationType::VMware | 
            VirtualizationType::VirtualBox |
            VirtualizationType::HyperV |
            VirtualizationType::Xen |
            VirtualizationType::Unknown
        )
    }
}

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
    /// Detect storage type for a path (with VM awareness)
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

        let device_name = device.trim_start_matches("/dev/");
        let base_device = Self::get_base_device(device_name);

        // 3. Detect virtualization
        let virt = VirtualizationType::detect();
        
        // 4. Use VM-specific heuristics if in a VM
        if virt.is_vm() {
            return Self::detect_in_vm(&base_device, virt);
        }
        
        // 5. Standard detection for bare metal
        Self::detect_bare_metal(&base_device)
    }
    
    /// Specialized detection for VMs
    fn detect_in_vm(base_device: &str, virt: VirtualizationType) -> Self {
        // VirtIO (vda, vdb, etc) - usually means fast backend (SSD/NVMe on host)
        // Hosts that configure VirtIO typically use high-performance storage
        if base_device.starts_with("vd") {
            info!("Autoconfig: VM detected ({:?}) with VirtIO disk - assuming SSD", virt);
            return StorageType::SSD;
        }
        
        // Xen virtual disk (xvda, xvdb)
        if base_device.starts_with("xvd") {
            info!("Autoconfig: Xen VM with paravirt disk - assuming SSD");
            return StorageType::SSD;
        }
        
        // NVMe passthrough or emulated in VM - it's real NVMe
        if base_device.starts_with("nvme") {
            return StorageType::NVMe;
        }
        
        // Emulated SCSI (sda) - check rotational, but with caution
        if base_device.starts_with("sd") {
            // In modern VMs, virtio-scsi SCSI is usually SSD
            let rotational_path = format!("/sys/block/{}/queue/rotational", base_device);
            if let Ok(content) = std::fs::read_to_string(&rotational_path) {
                match content.trim() {
                    "0" => return StorageType::SSD,
                    "1" => {
                        // In VMs, emulated HDD is rare - probably hypervisor lie
                        // Assume SSD for safety (better performance assumption)
                        warn!("Autoconfig: VM with rotational disk - may be inaccurate, assuming SSD");
                        return StorageType::SSD;
                    }
                    _ => {}
                }
            }
            
            // Fallback: in VMs assume SSD (most common config in 2024+)
            return StorageType::SSD;
        }
        
        // For other cases in VM, assume SSD (modern storage)
        info!("Autoconfig: VM detected, assuming SSD by default");
        StorageType::SSD
    }
    
    /// Detection for real hardware (bare metal)
    fn detect_bare_metal(base_device: &str) -> Self {
        // NVMe detection
        if base_device.starts_with("nvme") {
            return StorageType::NVMe;
        }

        // Check rotational flag - reliable on bare metal
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
            RamProfile::Low => 4000,        // 4s
            RamProfile::Medium => 3000,     // 3s
            RamProfile::Standard => 3000,   // 3s - better desktop protection
            RamProfile::High => 2000,       // 2s
            RamProfile::VeryHigh => 2000,   // 2s - workstations can have heavy loads too
        }
    }

    /// Recommended zswap compressor
    pub fn recommended_zswap_compressor(&self) -> &'static str {
        match self {
            RamProfile::UltraLow | RamProfile::Low => "zstd",
            _ => "lz4",
        }
    }

    /// Adjust MGLRU based on current memory pressure
    /// Call this at daemon startup to adapt to real conditions
    pub fn recommended_mglru_with_pressure(&self, pressure: crate::meminfo::MemoryPressure) -> u32 {
        use crate::meminfo::MemoryPressure;

        let base = self.recommended_mglru_min_ttl();

        // Increase protection under pressure
        match pressure {
            MemoryPressure::Low => base,
            MemoryPressure::Medium => base * 15 / 10,  // +50%
            MemoryPressure::High => base * 2,          // +100%
            MemoryPressure::Critical => base * 3,      // +200%
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
    
    // Swapfc settings
    pub swapfc_enabled: bool,
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
            swapfc_enabled: false,
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
            swapfc_enabled: false,
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
            swapfc_enabled: false,
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
            swapfc_enabled: false,
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
            swapfc_enabled: true,
            swapfc_directio: is_nvme,    // Direct I/O only on NVMe
            swapfc_chunk_size: if is_nvme { "1G" } else { "512M" }.to_string(),
            mglru_min_ttl_ms: ram.recommended_mglru_min_ttl(),
        }
    }

    fn for_fallback(ram: &RamProfile) -> Self {
        Self::for_live_system(ram)
    }
}
/// Information about a swap partition
#[derive(Debug, Clone)]
pub struct SwapPartition {
    /// Device path (e.g., /dev/sda2, /dev/nvme0n1p3)
    pub device: String,
    /// UUID of the partition
    pub uuid: Option<String>,
    /// Total size in bytes
    pub size_bytes: u64,
    /// Used bytes (0 if not active)
    pub used_bytes: u64,
    /// Storage type (for priority calculation)
    pub storage_type: StorageType,
    /// Whether currently activated as swap
    pub is_active: bool,
    /// Priority (from /proc/swaps if active)
    pub priority: i32,
}

impl SwapPartition {
    /// Calculate usage percentage
    pub fn usage_percent(&self) -> u8 {
        if self.size_bytes == 0 {
            return 0;
        }
        ((self.used_bytes * 100) / self.size_bytes) as u8
    }
}

/// Detect swap partitions on the system
/// Parses /proc/swaps for active partitions and lsblk for all swap-formatted partitions
pub fn detect_swap_partitions() -> Vec<SwapPartition> {
    let mut partitions = Vec::new();
    
    // 1. Get active swap partitions from /proc/swaps
    let active_swaps = get_active_swap_devices();
    
    // 2. Parse lsblk for all partitions with FSTYPE=swap
    if let Ok(output) = Command::new("lsblk")
        .args(["-b", "-n", "-o", "NAME,FSTYPE,SIZE,UUID,TYPE"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 4 {
                let name = fields[0];
                let fstype = fields[1];
                let size_str = fields[2];
                
                // Only process swap partitions
                if fstype != "swap" {
                    continue;
                }
                
                // Skip (skip any zram or loop devices - those are swapfiles)
                if name.starts_with("zram") || name.starts_with("loop") {
                    continue;
                }
                
                let device = format!("/dev/{}", name.trim_start_matches("├─").trim_start_matches("└─"));
                let uuid = if fields.len() >= 4 { Some(fields[3].to_string()) } else { None };
                let size_bytes: u64 = size_str.parse().unwrap_or(0);
                
                // Check if this partition is active
                let (is_active, used_bytes, priority) = active_swaps
                    .iter()
                    .find(|(d, _, _, _)| *d == device)
                    .map(|(_, used, _, prio)| (true, *used, *prio))
                    .unwrap_or((false, 0, 0));
                
                let storage_type = StorageType::detect(&device);
                
                partitions.push(SwapPartition {
                    device,
                    uuid,
                    size_bytes,
                    used_bytes,
                    storage_type,
                    is_active,
                    priority,
                });
            }
        }
    }
    
    // Sort by priority (higher first) then by storage type
    partitions.sort_by(|a, b| {
        b.priority.cmp(&a.priority)
            .then_with(|| storage_type_priority(&b.storage_type).cmp(&storage_type_priority(&a.storage_type)))
    });
    
    partitions
}

/// Get list of currently active swap devices from /proc/swaps
/// Returns: Vec<(device, used_bytes, size_bytes, priority)>
fn get_active_swap_devices() -> Vec<(String, u64, u64, i32)> {
    let mut devices = Vec::new();
    
    if let Ok(content) = std::fs::read_to_string("/proc/swaps") {
        for line in content.lines().skip(1) {  // Skip header
            let fields: Vec<&str> = line.split_whitespace().collect();
            // Format: Filename Type Size Used Priority
            if fields.len() >= 5 && fields[1] == "partition" {
                let device = fields[0].to_string();
                let size_kb: u64 = fields[2].parse().unwrap_or(0);
                let used_kb: u64 = fields[3].parse().unwrap_or(0);
                let priority: i32 = fields[4].parse().unwrap_or(0);
                
                devices.push((device, used_kb * 1024, size_kb * 1024, priority));
            }
        }
    }
    
    devices
}

/// Get priority weight for storage type (for sorting)
fn storage_type_priority(storage: &StorageType) -> u8 {
    match storage {
        StorageType::NVMe => 5,
        StorageType::SSD => 4,
        StorageType::EMMC => 3,
        StorageType::SD => 1,
        StorageType::HDD => 2,
        StorageType::Tmpfs => 6,
        StorageType::Unknown => 0,
    }
}

/// Get total swap partition capacity and usage
pub fn get_swap_partition_stats() -> (u64, u64) {
    let partitions = detect_swap_partitions();
    let total: u64 = partitions.iter().filter(|p| p.is_active).map(|p| p.size_bytes).sum();
    let used: u64 = partitions.iter().filter(|p| p.is_active).map(|p| p.used_bytes).sum();
    (total, used)
}