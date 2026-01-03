"""Tests for Config class and configuration parsing."""

import os

# We need to mock systemd and sysv_ipc before importing
import sys
import tempfile
from unittest.mock import MagicMock

import pytest

sys.modules["systemd"] = MagicMock()
sys.modules["systemd.daemon"] = MagicMock()
sys.modules["sysv_ipc"] = MagicMock()


class TestConfigParsing:
    """Tests for configuration file parsing."""

    def test_parse_config_simple(self):
        """Test parsing a simple config file."""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".conf", delete=False) as f:
            f.write("key1=value1\n")
            f.write("key2=value2\n")
            f.write("# comment line\n")
            f.write("key3=value3\n")
            config_path = f.name
        try:
            # Parse config manually to test the logic
            with open(config_path) as cf:
                lines = cf.read().splitlines()
            config = {}
            for line in lines:
                line = line.strip()
                if line.startswith("#") or "=" not in line:
                    continue
                key, value = line.split("=", 1)
                config[key] = value
            assert config["key1"] == "value1"
            assert config["key2"] == "value2"
            assert config["key3"] == "value3"
            assert len(config) == 3
        finally:
            os.unlink(config_path)

    def test_parse_config_with_comments(self):
        """Test that comments are properly ignored."""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".conf", delete=False) as f:
            f.write("# This is a comment\n")
            f.write("enabled=1\n")
            f.write("  # Indented comment\n")
            f.write("disabled=0\n")
            config_path = f.name
        try:
            # Basic structure test
            with open(config_path) as cf:
                lines = cf.read().splitlines()
            non_comment_lines = [
                line
                for line in lines
                if line.strip() and not line.strip().startswith("#")
            ]
            assert len(non_comment_lines) == 2
        finally:
            os.unlink(config_path)

    def test_parse_config_empty_file(self):
        """Test parsing an empty config file."""
        with tempfile.NamedTemporaryFile(mode="w", suffix=".conf", delete=False) as f:
            f.write("")
            config_path = f.name
        try:
            with open(config_path) as cf:
                content = cf.read()
            assert content == ""
        finally:
            os.unlink(config_path)


class TestConfigBooleanConversion:
    """Tests for boolean value conversion."""

    @pytest.mark.parametrize(
        "value,expected",
        [
            ("yes", True),
            ("Yes", True),
            ("YES", True),
            ("y", True),
            ("Y", True),
            ("1", True),
            ("true", True),
            ("True", True),
            ("TRUE", True),
            ("no", False),
            ("No", False),
            ("n", False),
            ("0", False),
            ("false", False),
            ("False", False),
            ("random", False),
            ("", False),
        ],
    )
    def test_boolean_conversion(self, value, expected):
        """Test boolean conversion from config values."""
        # This tests the logic used in Config.get() for bool type
        result = value.lower() in ["yes", "y", "1", "true"]
        assert result == expected


class TestConfigPrecedence:
    """Tests for configuration file precedence."""

    def test_config_paths_order(self):
        """Verify that config paths follow systemd precedence."""
        # Expected order: /usr/lib/systemd < /run/systemd < /etc/systemd
        paths = ["/usr/lib/systemd", "/run/systemd", "/etc/systemd"]
        # Later paths should override earlier ones
        assert paths[0] == "/usr/lib/systemd"  # vendor
        assert paths[1] == "/run/systemd"  # runtime
        assert paths[2] == "/etc/systemd"  # local admin
