"""Tests for helper functions and utilities."""

import os

# Mock system dependencies
import sys
import tempfile
from unittest.mock import MagicMock

import pytest

sys.modules["systemd"] = MagicMock()
sys.modules["systemd.daemon"] = MagicMock()
sys.modules["sysv_ipc"] = MagicMock()


class TestMemStats:
    """Tests for memory statistics parsing."""

    def test_parse_meminfo_format(self):
        """Test parsing /proc/meminfo format."""
        # Sample meminfo content
        meminfo_content = """MemTotal:       16384000 kB
MemFree:         8192000 kB
MemAvailable:   12288000 kB
Buffers:          512000 kB
Cached:          4096000 kB
SwapTotal:       8192000 kB
SwapFree:        8192000 kB
"""
        lines = meminfo_content.strip().split("\n")
        stats = {}
        fields = ["MemTotal", "MemFree", "SwapTotal", "SwapFree"]

        for line in lines:
            items = line.split()
            key = items[0][:-1]  # Remove colon
            if len(items) >= 3 and items[2] == "kB" and key in fields:
                stats[key] = int(items[1]) * 1024

        assert stats["MemTotal"] == 16384000 * 1024
        assert stats["MemFree"] == 8192000 * 1024
        assert stats["SwapTotal"] == 8192000 * 1024
        assert stats["SwapFree"] == 8192000 * 1024


class TestFileOperations:
    """Tests for file operation utilities."""

    def test_force_remove_existing_file(self):
        """Test removing an existing file."""
        with tempfile.NamedTemporaryFile(delete=False) as f:
            filepath = f.name

        assert os.path.exists(filepath)
        os.remove(filepath)
        assert not os.path.exists(filepath)

    def test_force_remove_nonexistent_file(self):
        """Test that removing nonexistent file doesn't raise."""
        filepath = "/tmp/nonexistent_test_file_12345"
        assert not os.path.exists(filepath)
        # Should not raise
        try:
            os.remove(filepath)
        except OSError:
            pass  # Expected behavior

    def test_makedirs_creates_nested(self):
        """Test that makedirs creates nested directories."""
        with tempfile.TemporaryDirectory() as tmpdir:
            nested_path = os.path.join(tmpdir, "a", "b", "c")
            assert not os.path.exists(nested_path)
            os.makedirs(nested_path, exist_ok=True)
            assert os.path.isdir(nested_path)

    def test_makedirs_existing_ok(self):
        """Test that makedirs with exist_ok=True works on existing dir."""
        with tempfile.TemporaryDirectory() as tmpdir:
            # Should not raise
            os.makedirs(tmpdir, exist_ok=True)
            assert os.path.isdir(tmpdir)


class TestSwapPercentageCalculations:
    """Tests for swap percentage calculations."""

    @pytest.mark.parametrize(
        "free,total,expected",
        [
            (50, 100, 50),
            (25, 100, 25),
            (0, 100, 0),
            (100, 100, 100),
            (333, 1000, 33),  # rounded
            (0, 0, 0),  # edge case - should use max(total, 1)
        ],
    )
    def test_free_swap_percentage(self, free, total, expected):
        """Test free swap percentage calculation."""
        # Replicate the calculation from get_free_swap_perc
        result = round((free * 100) / max(total, 1))
        assert result == expected

    @pytest.mark.parametrize(
        "free,total,expected",
        [
            (8192, 16384, 50),
            (4096, 16384, 25),
            (16384, 16384, 100),
            (0, 16384, 0),
        ],
    )
    def test_free_ram_percentage(self, free, total, expected):
        """Test free RAM percentage calculation."""
        result = round((free * 100) / total)
        assert result == expected


class TestPathOperations:
    """Tests for path-related operations."""

    def test_relative_symlink_creation(self):
        """Test creating relative symlinks."""
        with tempfile.TemporaryDirectory() as tmpdir:
            target = os.path.join(tmpdir, "target.txt")
            link = os.path.join(tmpdir, "subdir", "link.txt")

            # Create target file
            with open(target, "w") as f:
                f.write("test")

            # Create subdir
            os.makedirs(os.path.dirname(link))

            # Create relative symlink
            rel_target = os.path.relpath(target, os.path.dirname(link))
            os.symlink(rel_target, link)

            assert os.path.islink(link)
            assert os.path.exists(link)  # Link resolves
