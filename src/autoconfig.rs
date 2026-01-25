use crate::helpers::get_fstype;
use crate::{debug, info};

/// System capabilities
#[derive(Debug, Clone)]
pub struct SystemCapabilities {
    pub swap_path_fstype: Option<String>,
}

impl SystemCapabilities {
    /// Detect system capabilities
    pub fn detect() -> Self {
        let swap_path_fstype = get_fstype("/swapfc").or_else(|| get_fstype("/"));
        debug!("Detected swap path filesystem: {:?}", swap_path_fstype);
        Self { swap_path_fstype }
    }
}

/// Recommended swap configuration
#[derive(Debug, Clone, Default)]
pub struct RecommendedConfig {
    pub use_zswap: bool,
    pub use_swapfc: bool,
}

impl RecommendedConfig {
    /// Generate recommended configuration based on system capabilities
    pub fn from_capabilities(caps: &SystemCapabilities) -> Self {
        let mut config = Self::default();

        // Check if filesystem supports swap files well (btrfs, ext4, xfs)
        let supports_swapfiles = caps.swap_path_fstype.as_deref()
            .map(|fs| matches!(fs, "btrfs" | "ext4" | "xfs"))
            .unwrap_or(false);

        if supports_swapfiles {
            // zswap + swapfc is the best option for btrfs/ext4/xfs
            info!("Filesystem supports swapfiles: using zswap + swapfc");
            config.use_zswap = true;
            config.use_swapfc = true;
        } else {
            // zram only for unsupported filesystems
            info!("Filesystem does not support swapfiles well: using zram only");
            config.use_zswap = false;
            config.use_swapfc = false;
        }

        config
    }
}
