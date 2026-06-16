#!/bin/bash
# MyMem installer — build the Rust binary and wire it into Claude on this machine.
# Sets up: mymem binary (~/.local/bin), shell alias, project symlinks, auto-sync hooks.
set -euo pipefail

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
SM="$HOME/.claude/mymem"
BIN="$HOME/.local/bin/mymem"
LOG="$HOME/.claude/mymem.log"

# ---------- platform detection ----------
OS="$(uname -s 2>/dev/null || echo unknown)"
IS_WSL=0
if grep -qiE 'microsoft|wsl' /proc/version 2>/dev/null || [ -n "${WSL_DISTRO_NAME:-}" ]; then
  IS_WSL=1
  echo "→ WSL2 detected (${WSL_DISTRO_NAME:-wsl})"
fi

# Wire aliases into the user's actual login shell rc (zsh on macOS, bash on WSL2).
case "$(basename "${SHELL:-}")" in
  zsh)  SHELL_RC="$HOME/.zshrc" ;;
  bash) SHELL_RC="$HOME/.bashrc" ;;
  *)    if [ "$OS" = "Darwin" ]; then SHELL_RC="$HOME/.zshrc"; else SHELL_RC="$HOME/.bashrc"; fi ;;
esac

# ---------- preflight (validate everything BEFORE touching the system) ----------
echo "→ preflight"
need_fail=0
for dep in rsync python3; do
  command -v "$dep" >/dev/null 2>&1 || { echo "  ✗ $dep not found (required)"; need_fail=1; }
done

# Build path: cargo to compile, or a macOS-only prebuilt binary. No Linux/Windows
# prebuilt ships yet, so WSL2/Linux must compile — which needs a C linker (cc).
if command -v cargo >/dev/null 2>&1; then
  if [ "$OS" = "Linux" ] && ! command -v cc >/dev/null 2>&1; then
    echo "  ✗ C linker 'cc' not found — Rust needs a system C toolchain to build"; need_fail=1
  fi
elif [ "$OS" = "Darwin" ] && [ -f "$SELF_DIR/target/release/mymem" ]; then
  : # macOS prebuilt binary present — ok
else
  echo "  ✗ cargo not found and no usable prebuilt for $OS — install Rust: https://rustup.rs"; need_fail=1
fi

# Warn-only: needed for cross-machine sync, not for a single-machine install.
command -v ssh >/dev/null 2>&1 || echo "  ⚠ ssh not found — needed for peer sync; install before adding peers"
command -v tailscale >/dev/null 2>&1 || echo "  ⚠ tailscale not found — required for cross-machine sync over the tailnet"

# WSL2 + Windows desktop app: ~/.claude here is the WSL home, NOT where the
# Windows Claude Code writes. Surface the real path so hooks don't land nowhere.
if [ "$IS_WSL" = 1 ] && [ ! -d "$HOME/.claude" ]; then
  win="$(ls -d /mnt/c/Users/*/.claude 2>/dev/null | head -1 || true)"
  if [ -n "$win" ]; then
    echo "  ⚠ ~/.claude not found in WSL2, but Claude Code data exists at:"
    echo "      $win"
    echo "    mymem will configure the WSL-side ~/.claude. Run Claude Code *inside WSL2*"
    echo "    so it reads the same path, otherwise the hooks land in the wrong settings.json."
  fi
fi

if [ "$need_fail" = 1 ]; then
  [ "$OS" = "Linux" ] && echo "  fix on WSL2/Debian/Ubuntu:  sudo apt-get update && sudo apt-get install -y rsync openssh-client python3 build-essential" >&2
  echo "  preflight failed — nothing was changed." >&2
  exit 1
fi

echo "→ build mymem (Rust)"
mkdir -p "$SM" "$HOME/.claude/projects" "$HOME/.local/bin"
if command -v cargo >/dev/null 2>&1; then
  ( cd "$SELF_DIR" && cargo build --release )
  cp "$SELF_DIR/target/release/mymem" "$BIN"
elif [ -f "$SELF_DIR/target/release/mymem" ] && [ "$OS" = "Darwin" ]; then
  echo "  cargo not found — using prebuilt binary (macOS Apple Silicon)"
  cp "$SELF_DIR/target/release/mymem" "$BIN"
else
  echo "  ERROR: no cargo and no usable prebuilt binary for $OS." >&2
  echo "  Install Rust (https://rustup.rs) and re-run — mymem compiles natively on WSL2/Linux." >&2
  exit 1
fi
chmod +x "$BIN"

echo "→ shell aliases (mymem, mn) → $SHELL_RC"
grep -q 'alias mymem=' "$SHELL_RC" 2>/dev/null || echo 'alias mymem="$HOME/.local/bin/mymem"' >> "$SHELL_RC"
grep -q 'alias mn=' "$SHELL_RC" 2>/dev/null || echo 'alias mn="$HOME/.local/bin/mymem"' >> "$SHELL_RC"

echo "→ peer config (~/.claude/mymem.conf)"
CONF="$HOME/.claude/mymem.conf"
if [ ! -s "$CONF" ]; then
  if [ -n "${MYMEM_PEER:-}" ]; then
    echo "$MYMEM_PEER" > "$CONF"; echo "  peer = $MYMEM_PEER"
  else
    printf "  peer (user@tailscale-host of the OTHER machine), blank to skip: "
    read -r ans </dev/tty 2>/dev/null || ans=""
    if [ -n "$ans" ]; then echo "$ans" > "$CONF"; else echo "  skipped — set later:  echo user@host > $CONF"; fi
  fi
fi

echo "→ symlink all existing project memory dirs into the pool"
for proj in "$HOME"/.claude/projects/*/; do
  m="${proj}memory"
  [ -L "$m" ] && continue
  if [ -e "$m" ]; then
    rsync -a --exclude MEMORY.md "$m/" "$SM/"
    mv "$m" "${m}.bak.$(date +%Y%m%d-%H%M%S)"
  fi
  ln -s "$SM" "$m"
done

echo "→ slash command (/recall)"
if [ -f "$SELF_DIR/.claude/commands/recall.md" ]; then
  mkdir -p "$HOME/.claude/commands"
  cp "$SELF_DIR/.claude/commands/recall.md" "$HOME/.claude/commands/recall.md"
fi

echo "→ auto-sync hooks (SessionStart=start, Stop=push)"
BIN="$BIN" LOG="$LOG" python3 - <<'PY'
import json, os
p=os.path.expanduser("~/.claude/settings.json")
d=json.load(open(p)) if os.path.exists(p) else {}
h=d.setdefault("hooks",{})
b=os.environ["BIN"]; log=os.environ["LOG"]
def has(ev): return any("mymem" in hk.get("command","") for g in h.get(ev,[]) for hk in g.get("hooks",[]))
if not has("SessionStart"):
    h.setdefault("SessionStart",[]).append({"hooks":[{"type":"command","command":f"{b} start >> {log} 2>&1"}]})
if not has("Stop"):
    h.setdefault("Stop",[]).append({"hooks":[{"type":"command","command":f"nohup {b} push >> {log} 2>&1 &"}]})
json.dump(d,open(p,"w"),indent=2,ensure_ascii=False); print("  hooks ok")
PY

echo ""
echo "✓ MyMem installed on $(hostname -s) — try: mymem doctor"
echo "  peer is read from ~/.claude/mymem.conf (or env MYMEM_PEER)"
echo "  reload your shell:  source $SHELL_RC"
if [ "$IS_WSL" = 1 ]; then
  echo ""
  echo "  WSL2 note: mymem manages ~/.claude/mymem inside this distro."
  echo "  Run Claude Code *inside WSL2* so it shares the same ~/.claude."
  echo "  The Windows desktop app keeps memory on C:\\Users\\<you>\\.claude and is NOT bridged."
fi
