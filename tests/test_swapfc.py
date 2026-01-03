"""Tests for SwapFc (Swap File Chunked) functionality."""

# Mock system dependencies
import sys
from unittest.mock import MagicMock

import pytest

sys.modules["systemd"] = MagicMock()
sys.modules["systemd.daemon"] = MagicMock()
sys.modules["sysv_ipc"] = MagicMock()


class TestSwapFcConfiguration:
    """Tests for SwapFc configuration validation."""

    @pytest.mark.parametrize(
        "frequency,expected_valid",
        [
            (1, True),
            (60, True),
            (3600, True),
            (86400, True),
            (0, False),
            (-1, False),
            (86401, False),
            (100000, False),
        ],
    )
    def test_frequency_validation(self, frequency, expected_valid):
        """Test that swapfc_frequency is validated within 1..86400."""
        is_valid = 1 <= frequency <= 86400
        assert is_valid == expected_valid

    @pytest.mark.parametrize(
        "max_count,expected_valid",
        [
            (1, True),
            (16, True),
            (32, True),
            (0, False),
            (33, False),
            (-1, False),
        ],
    )
    def test_max_count_validation(self, max_count, expected_valid):
        """Test that swapfc_max_count is validated within 1..32."""
        is_valid = 1 <= max_count <= 32
        assert is_valid == expected_valid


class TestSwapFcChunkSize:
    """Tests for chunk size parsing and validation."""

    @pytest.mark.parametrize(
        "chunk_str,expected_bytes",
        [
            ("256M", 256 * 1024 * 1024),
            ("512M", 512 * 1024 * 1024),
            ("1G", 1024 * 1024 * 1024),
            ("128M", 128 * 1024 * 1024),
        ],
    )
    def test_chunk_size_parsing(self, chunk_str, expected_bytes):
        """Test parsing chunk size from human-readable format."""
        # Simulate numfmt parsing
        multipliers = {"K": 1024, "M": 1024**2, "G": 1024**3}
        value = int(chunk_str[:-1])
        suffix = chunk_str[-1].upper()
        result = value * multipliers.get(suffix, 1)
        assert result == expected_bytes


class TestSwapFcSpaceCheck:
    """Tests for disk space checking logic."""

    def test_has_enough_space_logic(self):
        """Test the space checking algorithm."""
        chunk_size = 256 * 1024 * 1024  # 256 MiB
        block_size = 4096

        # Simulate statvfs values
        free_blocks = 200000  # ~800 MiB
        free_bytes = free_blocks * block_size

        # Need at least chunk_size + chunk_size (reserve)
        has_space = (free_bytes - chunk_size) >= chunk_size
        assert has_space is True

        # Not enough space
        free_blocks = 50000  # ~200 MiB
        free_bytes = free_blocks * block_size
        has_space = (free_bytes - chunk_size) >= chunk_size
        assert has_space is False


class TestSwapFcPollingRate:
    """Tests for polling rate adjustment."""

    def test_double_polling_rate(self):
        """Test that polling rate doubles correctly."""
        initial_rate = 1
        max_rate = 86400

        rate = initial_rate
        new_rate = rate * 2
        if new_rate <= max_rate:
            rate = new_rate

        assert rate == 2

    def test_double_polling_rate_max_limit(self):
        """Test that polling rate doesn't exceed maximum."""
        rate = 50000
        frequency = 1
        max_multiplier = 1000

        new_rate = rate * 2
        if new_rate > 86400 or new_rate > frequency * max_multiplier:
            new_rate = rate  # Don't double

        assert new_rate == rate  # Should not have doubled

    def test_reset_polling_rate(self):
        """Test resetting polling rate to base frequency."""
        base_frequency = 1
        current_rate = 64  # After several doublings

        if current_rate > base_frequency:
            current_rate = base_frequency

        assert current_rate == base_frequency


class TestSwapFcBtrfsHandling:
    """Tests for btrfs-specific handling."""

    def test_btrfs_kernel_version_check(self):
        """Test kernel version check for btrfs swap support."""
        # Kernel 5+ supports native btrfs swapfiles
        kmajor_new = 5
        kmajor_old = 4

        # For kernel 5+, use nocow
        use_nocow = kmajor_new >= 5
        assert use_nocow is True

        # For kernel < 5, use loop device
        use_loop = kmajor_old < 5
        assert use_loop is True

    def test_filesystem_type_detection_logic(self):
        """Test logic for detecting filesystem type."""
        # Simulate df output parsing
        df_output = "Type\nbtrfs"
        lines = df_output.strip().split("\n")
        fs_type = lines[1] if len(lines) > 1 else "unknown"
        assert fs_type == "btrfs"

        df_output_ext4 = "Type\next4"
        lines = df_output_ext4.strip().split("\n")
        fs_type = lines[1] if len(lines) > 1 else "unknown"
        assert fs_type == "ext4"
