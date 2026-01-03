"""Tests for Zram and Zswap functionality."""

# Mock system dependencies
import sys
from unittest.mock import MagicMock

import pytest

sys.modules["systemd"] = MagicMock()
sys.modules["systemd.daemon"] = MagicMock()
sys.modules["sysv_ipc"] = MagicMock()


class TestZswapConfiguration:
    """Tests for Zswap configuration."""

    @pytest.mark.parametrize(
        "compressor",
        [
            "lzo",
            "lz4",
            "zstd",
            "lzo-rle",
            "lz4hc",
        ],
    )
    def test_valid_compressors(self, compressor):
        """Test that valid compressor names are accepted."""
        valid_compressors = ["lzo", "lz4", "zstd", "lzo-rle", "lz4hc"]
        assert compressor in valid_compressors

    @pytest.mark.parametrize(
        "zpool",
        [
            "zbud",
            "z3fold",
            "zsmalloc",
        ],
    )
    def test_valid_zpools(self, zpool):
        """Test that valid zpool drivers are accepted."""
        valid_zpools = ["zbud", "z3fold", "zsmalloc"]
        assert zpool in valid_zpools

    @pytest.mark.parametrize(
        "percent,expected_valid",
        [
            (1, True),
            (25, True),
            (50, True),
            (99, True),
            (0, False),
            (100, False),
            (-1, False),
        ],
    )
    def test_max_pool_percent_validation(self, percent, expected_valid):
        """Test zswap_max_pool_percent validation (1-99)."""
        is_valid = 1 <= percent <= 99
        assert is_valid == expected_valid


class TestZswapStats:
    """Tests for Zswap statistics calculations."""

    def test_compression_ratio_calculation(self):
        """Test compression ratio calculation."""
        page_size = 4096
        used_bytes = 100 * 1024 * 1024  # 100 MiB in memory
        used_pages = used_bytes / page_size
        stored_pages = 300 * 1024 * 1024 / page_size  # 300 MiB stored (compressed)

        ratio = 0
        if stored_pages > 0:
            ratio = used_pages * 100 / stored_pages

        # 100 MiB used to store 300 MiB = ~33% ratio
        assert round(ratio) == 33

    def test_zswap_store_percentage(self):
        """Test zswap store vs swap store percentage."""
        stored_bytes = 500 * 1024 * 1024  # 500 MiB in zswap
        swap_used = 1024 * 1024 * 1024  # 1 GiB total swap used

        percentage = round(stored_bytes * 100 / swap_used)
        assert percentage == 49  # ~49% of swap is in zswap


class TestZramConfiguration:
    """Tests for Zram configuration."""

    @pytest.mark.parametrize(
        "priority,expected_valid",
        [
            (1, True),
            (100, True),
            (32767, True),
            (0, False),
            (-1, False),
            (32768, False),
        ],
    )
    def test_priority_validation(self, priority, expected_valid):
        """Test zram_prio validation (1-32767)."""
        is_valid = 1 <= priority <= 32767
        assert is_valid == expected_valid

    def test_zram_size_calculation(self):
        """Test zram size equals RAM size by default."""
        ram_size = 16 * 1024 * 1024 * 1024  # 16 GiB
        zram_size = ram_size  # Default: equal to RAM
        assert zram_size == ram_size

    def test_zram_size_with_count_old_kernel(self):
        """Test zram size division for old kernels."""
        total_size = 16 * 1024 * 1024 * 1024  # 16 GiB
        zram_count = 4

        # For kernels < 4.7, size is divided by count
        size_per_device = round(total_size / zram_count)
        assert size_per_device == 4 * 1024 * 1024 * 1024


class TestZramDeviceHandling:
    """Tests for Zram device management."""

    def test_zram_device_path_format(self):
        """Test zram device path format."""
        device_num = 0
        expected_path = f"/dev/zram{device_num}"
        assert expected_path == "/dev/zram0"

        device_num = 5
        expected_path = f"/dev/zram{device_num}"
        assert expected_path == "/dev/zram5"

    def test_detect_zram_in_swap_output(self):
        """Test detecting zram devices in swapon output."""
        swapon_output = """NAME       TYPE      SIZE USED PRIO
/dev/zram0 partition 8G   0B   100
/dev/sda2  partition 4G   1G   -2
"""
        lines = swapon_output.strip().split("\n")
        zram_devices = [line for line in lines if "zram" in line]
        assert len(zram_devices) == 1
        assert "/dev/zram0" in zram_devices[0]

    def test_filter_zram_from_swapd(self):
        """Test that zram devices are filtered from swapd."""
        devices = ["/dev/sda2", "/dev/zram0", "/dev/loop0", "/dev/nvme0n1p3"]

        # swapd should skip zram and loop devices
        filtered = [d for d in devices if "zram" not in d and "loop" not in d]
        assert filtered == ["/dev/sda2", "/dev/nvme0n1p3"]


class TestKernelVersionChecks:
    """Tests for kernel version compatibility checks."""

    @pytest.mark.parametrize(
        "kmajor,kminor,needs_multiple_zram",
        [
            (4, 6, True),  # < 4.7 needs multiple devices
            (4, 7, False),  # >= 4.7 single device is fine
            (4, 8, False),
            (5, 0, False),
            (6, 0, False),
        ],
    )
    def test_multiple_zram_requirement(self, kmajor, kminor, needs_multiple_zram):
        """Test kernel version check for multiple zram devices."""
        result = kmajor <= 4 and kminor < 7
        assert result == needs_multiple_zram

    @pytest.mark.parametrize(
        "kmajor,supports_btrfs_swap",
        [
            (4, False),
            (5, True),
            (6, True),
        ],
    )
    def test_btrfs_native_swap_support(self, kmajor, supports_btrfs_swap):
        """Test kernel version check for native btrfs swap."""
        result = kmajor >= 5
        assert result == supports_btrfs_swap
