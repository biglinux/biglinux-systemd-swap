# systemd-swap

Dynamic swap management daemon for Linux, written in Rust.

Manages:
- **Zswap** - Compressed swap cache in RAM
- **Zram** - Compressed RAM disk for swap
- **SwapFC** - Dynamic swap file allocation on btrfs

## Installation

### Arch Linux / BigLinux

```bash
cd pkgbuild
makepkg -si
```

### Manual

Requires: `rust`, `cargo`, `systemd`

```bash
cargo build --release
sudo make install
```

## Usage

```bash
# Enable and start
sudo systemctl enable --now systemd-swap

# Check status
systemd-swap status

# View compression algorithms
systemd-swap compression
```

## Configuration

Edit `/etc/systemd/swap.conf`:

```ini
# Zswap (compressed RAM cache)
zswap_enabled=1
zswap_compressor=zstd
zswap_max_pool_percent=1
zswap_zpool=zsmalloc

# Zram (RAM disk swap)
zram_enabled=1
zram_size=$RAM_SIZE
zram_alg=zstd
zram_prio=32767

# SwapFC (dynamic swap files on btrfs)
swapfc_enabled=1
swapfc_chunk_size=512M
swapfc_max_count=32
swapfc_free_ram_perc=35
swapfc_path=/swapfc/swapfile
```

## FAQ

**Q: Should I use zram AND zswap?**  
A: No. Use one or the other. Both together can cause LRU inversion.

**Q: What filesystem does SwapFC support?**  
A: Btrfs only. Swap files are created as subvolumes with COW disabled.

**Q: Does this work with hibernation?**  
A: No. Dynamic swap files are not compatible with hibernation.

## File Locations

```
/usr/bin/systemd-swap          # Main binary
/etc/systemd/swap.conf         # Configuration
/usr/lib/systemd/system/       # Service files
/run/systemd/swap/             # Runtime data
```

## License

GPL-3.0-or-later
