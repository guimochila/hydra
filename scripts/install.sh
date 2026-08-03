#!/usr/bin/env bash
#
# Build hydra, put the binary somewhere stable, and register it with Claude Code +
# tmux.
#
# Why a script instead of just `cargo run -- install`: `hydra install` bakes the
# path of the *running* binary into ~/.claude/settings.json and ~/.tmux.conf. Run
# it straight out of ./target and every hook points into the build directory, so a
# `cargo clean` (or a checkout on another machine) silently breaks every hook. This
# copies the binary to a stable prefix first, then registers *that* copy.
#
#   ./scripts/install.sh                 # build + install to ~/.local/bin
#   ./scripts/install.sh --prefix /usr/local
#   ./scripts/install.sh --no-build      # register an already-built binary
#   ./scripts/install.sh --uninstall     # remove hooks, tmux block, and binary
#   ./scripts/install.sh --uninstall --purge   # also delete config + runtime state
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BUILD=1
UNINSTALL=0
PURGE=0

die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info() { printf '\033[36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarn:\033[0m %s\n' "$*" >&2; }

usage() {
    sed -n '3,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)    PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
        --prefix=*)  PREFIX="${1#*=}"; shift ;;
        --no-build)  BUILD=0; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        --purge)     PURGE=1; shift ;;
        -h|--help)   usage 0 ;;
        *)           printf 'unknown option: %s\n\n' "$1" >&2; usage 2 ;;
    esac
done

BIN_DIR="$PREFIX/bin"
TARGET="$BIN_DIR/hydra"

# ---- uninstall ------------------------------------------------------------------

if [ "$UNINSTALL" = 1 ]; then
    # Deregister with whichever hydra is actually installed: prefer the one at the
    # prefix, fall back to PATH. `hydra uninstall` must run *before* the binary is
    # deleted — it is what strips the hooks and the tmux block.
    HYDRA="$TARGET"
    [ -x "$HYDRA" ] || HYDRA="$(command -v hydra 2>/dev/null || true)"

    if [ -n "$HYDRA" ] && [ -x "$HYDRA" ]; then
        info "deregistering hooks and tmux binding ($HYDRA)"
        "$HYDRA" uninstall
    else
        warn "no hydra binary found — hooks in ~/.claude/settings.json may remain"
    fi

    if [ -f "$TARGET" ]; then
        info "removing $TARGET"
        rm -f "$TARGET"
    fi

    if [ "$PURGE" = 1 ]; then
        # `hydra uninstall` deliberately never deletes the user's config; --purge is
        # the explicit opt-in.
        CONFIG="${HYDRA_CONFIG:-$HOME/.config/hydra/config.toml}"
        [ -f "$CONFIG" ] && { info "removing $CONFIG"; rm -f "$CONFIG"; }
        STATE_DIR="${XDG_RUNTIME_DIR:+$XDG_RUNTIME_DIR/hydra}"
        STATE_DIR="${STATE_DIR:-${TMPDIR:-/tmp}/hydra-$(id -un)}"
        [ -d "$STATE_DIR" ] && { info "removing state dir $STATE_DIR"; rm -rf "$STATE_DIR"; }
    fi

    info "done. Reload tmux to drop the keybinding: tmux source-file ~/.tmux.conf"
    exit 0
fi

# ---- install --------------------------------------------------------------------

if [ "$BUILD" = 1 ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo not found — install Rust from https://rustup.rs"
    info "building release binary"
    (cd "$REPO_ROOT" && cargo build --release)
fi

BUILT="$REPO_ROOT/target/release/hydra"
[ -x "$BUILT" ] || die "no binary at $BUILT (drop --no-build to build it)"

mkdir -p "$BIN_DIR"
# Stage + mv rather than cp: overwriting a *running* binary in place fails with
# ETXTBSY, and a half-copied hydra would break every hook until the copy finished.
# rename(2) swaps it atomically and lets running processes keep the old inode.
STAGE="$BIN_DIR/.hydra.$$.tmp"
trap 'rm -f "$STAGE"' EXIT
cp "$BUILT" "$STAGE"
chmod 755 "$STAGE"
mv -f "$STAGE" "$TARGET"
trap - EXIT
info "installed binary → $TARGET"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) warn "$BIN_DIR is not on \$PATH — add it so 'hydra' works from a shell" ;;
esac

# Registers *this* copy: install.rs resolves the hook/tmux command from
# current_exe(), so it must be invoked through $TARGET, never through ./target.
info "registering Claude Code hooks and tmux binding"
"$TARGET" install

info "health check"
"$TARGET" doctor || die "doctor reported a problem — see above"

cat <<EOF

Next steps:
  tmux source-file ~/.tmux.conf   # pick up the popup keybinding
  hydra doctor                    # re-check anytime

Already-running Claude Code sessions keep the old hook config until restarted.
EOF
