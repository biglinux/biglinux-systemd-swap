#!/usr/bin/env bats
#
# Regression test for fstab_append_idempotent() in src/pre-systemd-swap.
# Verifies:
#   - 3 invocations with the same line produce exactly 1 occurrence
#   - original file mode is preserved
#   - no leftover tempfiles remain after a successful run, a signal or a
#     failed rename
#
# Run manually:
#   bats tests/fstab_idempotent.bats
#
# The signal and rename tests put shims on PATH from mktemp -d. On a host
# where /tmp is mounted noexec the shims never run and both tests fail; point
# TMPDIR at an executable directory there.
#

setup() {
    BATS_TMPDIR_CASE="$(mktemp -d)"
    FSTAB="${BATS_TMPDIR_CASE}/fstab"
    : > "$FSTAB"
    chmod 0644 "$FSTAB"

    # Extract the function from the production script (single source of truth).
    SCRIPT="$(cd "${BATS_TEST_DIRNAME}/.." && pwd)/src/pre-systemd-swap"
    [ -f "$SCRIPT" ] || skip "pre-systemd-swap script not found at $SCRIPT"

    # shellcheck disable=SC1090
    # Source the function definition only — avoid running the whole script.
    # Extract lines between the function header and its closing brace.
    HELPER_FILE="${BATS_TMPDIR_CASE}/helper.sh"
    # Match end BEFORE stripping indentation, otherwise the closing `    }`
    # pattern would never fire (sub() runs first and removes the 4-space
    # prefix). End marker uses an exact-match regex against the un-stripped
    # line.
    awk '
        /^    fstab_append_idempotent\(\) \{/ { in_fn=1 }
        in_fn {
            line = $0
            sub(/^    /, "", line)
            print line
            if ($0 == "    }") { exit }
        }
    ' "$SCRIPT" > "$HELPER_FILE"

    # shellcheck disable=SC1090
    source "$HELPER_FILE"
}

teardown() {
    [ -n "${BATS_TMPDIR_CASE:-}" ] && rm -rf "$BATS_TMPDIR_CASE"
}

@test "fstab_append_idempotent: 3 calls produce exactly 1 line" {
    LINE="UUID=deadbeef /swapfile btrfs subvol=/@swapfile,defaults,noatime 0 0"
    fstab_append_idempotent "$LINE" "$FSTAB"
    fstab_append_idempotent "$LINE" "$FSTAB"
    fstab_append_idempotent "$LINE" "$FSTAB"

    count=$(grep -cxF -- "$LINE" "$FSTAB")
    [ "$count" -eq 1 ]
}

@test "fstab_append_idempotent: preserves file mode" {
    chmod 0640 "$FSTAB"
    LINE="UUID=deadbeef /swapfile btrfs subvol=/@swapfile,defaults,noatime 0 0"
    fstab_append_idempotent "$LINE" "$FSTAB"

    mode=$(stat -c '%a' "$FSTAB")
    [ "$mode" = "640" ]
}

@test "fstab_append_idempotent: leaves no tempfiles" {
    LINE="UUID=cafef00d /swapfile btrfs subvol=/@swapfile,defaults,noatime 0 0"
    fstab_append_idempotent "$LINE" "$FSTAB"

    # No fstab.XXXXXX tempfiles in the same dir.
    leftovers=$(find "$(dirname "$FSTAB")" -maxdepth 1 -name 'fstab.??????' | wc -l)
    [ "$leftovers" -eq 0 ]
}

@test "fstab_append_idempotent: appends when file ends without newline" {
    printf 'existing-line-no-newline' > "$FSTAB"
    LINE="UUID=1234 /swapfile btrfs subvol=/@swapfile,defaults,noatime 0 0"
    fstab_append_idempotent "$LINE" "$FSTAB"

    count=$(grep -cxF -- "$LINE" "$FSTAB")
    [ "$count" -eq 1 ]
    # Existing content must still be present.
    grep -qF 'existing-line-no-newline' "$FSTAB"
}

# Signal mid-write -> tempfile cleaned up by trap, fstab untouched.
# SIGTERM, not SIGINT: bash starts `&` jobs of a non-interactive shell with
# SIGINT ignored, and a signal ignored on entry cannot be trapped, so an INT
# never reached the function and the line was appended anyway. setsid gives
# the job its own process group, so the signal also reaches the inner subshell
# that owns the trap and the stalled `cat`, not just the outer wrapper.
@test "fstab_append_idempotent: SIGTERM mid-write cleans tempfile" {
    LINE="UUID=sigterm /swapfile btrfs subvol=/@swap,defaults,noatime 0 0"

    # shim cat to stall. function uses `cat "$fstab" > "$tmp"` to seed.
    SHIM_DIR="${BATS_TMPDIR_CASE}/shim"
    mkdir -p "$SHIM_DIR"
    cat > "$SHIM_DIR/cat" <<'EOF'
#!/bin/bash
# stall long enough to be interrupted
sleep 3
exec /usr/bin/cat "$@"
EOF
    chmod +x "$SHIM_DIR/cat"

    # A session of its own, so the signal reaches the function's subshell and
    # the stalled `cat` too. setsid forks when it is already a group leader,
    # so the group id is the inner shell's $$, not $!; the shell records it.
    PGID_FILE="${BATS_TMPDIR_CASE}/pgid"
    # shellcheck disable=SC2016  # expanded by the inner bash
    setsid -w bash -c 'echo $$ > "$5"; source "$1"; PATH="$2:$PATH"; fstab_append_idempotent "$3" "$4"' \
        _ "$HELPER_FILE" "$SHIM_DIR" "$LINE" "$FSTAB" "$PGID_FILE" &
    pid=$!
    # Wait until the tempfile exists, so the signal lands mid-write.
    for _ in $(seq 50); do
        [ -s "$PGID_FILE" ] &&
            [ -n "$(find "$(dirname "$FSTAB")" -maxdepth 1 -name 'fstab.??????')" ] && break
        sleep 0.05
    done
    kill -TERM -- "-$(cat "$PGID_FILE")"
    wait "$pid" || true

    leftovers=$(find "$(dirname "$FSTAB")" -maxdepth 1 -name 'fstab.??????' | wc -l)
    [ "$leftovers" -eq 0 ]
    run grep -qF -- "$LINE" "$FSTAB"
    [ "$status" -ne 0 ]
}

# Failed rename -> function errors, no partial write, original intact.
# A shimmed `mv` fails for root too; read-only permission bits do not stop root,
# so the chmod-based version of this test failed whenever it ran as root.
@test "fstab_append_idempotent: failed rename errors, no partial write" {
    ORIG="pre-existing-content"
    printf '%s\n' "$ORIG" > "$FSTAB"

    SHIM_DIR="${BATS_TMPDIR_CASE}/shim"
    mkdir -p "$SHIM_DIR"
    printf '#!/bin/sh\nexit 1\n' > "$SHIM_DIR/mv"
    chmod +x "$SHIM_DIR/mv"

    LINE="UUID=ro /swapfile btrfs subvol=/@swap,defaults,noatime 0 0"
    # A prefix assignment does not reach the command bats' `run` executes, so
    # the shim has to be on PATH for the call itself.
    PATH="$SHIM_DIR:$PATH"
    run fstab_append_idempotent "$LINE" "$FSTAB"
    PATH="${PATH#"$SHIM_DIR":}"
    [ "$status" -ne 0 ]

    grep -qxF "$ORIG" "$FSTAB"
    run grep -qF -- "$LINE" "$FSTAB"
    [ "$status" -ne 0 ]
    leftovers=$(find "$(dirname "$FSTAB")" -maxdepth 1 -name 'fstab.??????' | wc -l)
    [ "$leftovers" -eq 0 ]
}

# Existing line with trailing whitespace differs by bytes, so guard does
# NOT fire on exact-match. But if the *exact* line (with trailing ws) is
# present, the guard must dedupe. Verify both halves of idempotency.
@test "fstab_append_idempotent: trailing whitespace exact-match dedups" {
    LINE="UUID=ws /swapfile btrfs subvol=/@swap,defaults,noatime 0 0   "
    # seed file with the exact same line (trailing spaces included)
    printf '%s\n' "$LINE" > "$FSTAB"

    fstab_append_idempotent "$LINE" "$FSTAB"
    fstab_append_idempotent "$LINE" "$FSTAB"

    # exactly one occurrence — grep -xF must match the trailing-ws line
    count=$(grep -cxF -- "$LINE" "$FSTAB")
    [ "$count" -eq 1 ]
}
