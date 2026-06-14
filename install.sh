#!/bin/bash
# Memnir installer — build the Rust binary and wire it into Claude on this machine.
# Sets up: memnir binary (~/.local/bin), shell alias, project symlinks, auto-sync hooks.
set -euo pipefail

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
SM="$HOME/.claude/memnir"
BIN="$HOME/.local/bin/memnir"
LOG="$HOME/.claude/memnir.log"

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

echo "→ check dependencies"
need_fail=0
for dep in rsync python3; do
  command -v "$dep" >/dev/null 2>&1 || { echo "  ✗ $dep not found (required)"; need_fail=1; }
done
command -v ssh >/dev/null 2>&1 || echo "  ⚠ ssh not found — needed for peer sync; install before adding peers"
if [ "$need_fail" = 1 ]; then
  [ "$OS" = "Linux" ] && echo "  on WSL2/Debian/Ubuntu:  sudo apt-get update && sudo apt-get install -y rsync openssh-client python3" >&2
  exit 1
fi

echo "→ build memnir (Rust)"
mkdir -p "$SM" "$HOME/.claude/projects" "$HOME/.local/bin"
if command -v cargo >/dev/null 2>&1; then
  ( cd "$SELF_DIR" && cargo build --release )
  cp "$SELF_DIR/target/release/memnir" "$BIN"
elif [ -f "$SELF_DIR/target/release/memnir" ] && [ "$OS" = "Darwin" ]; then
  echo "  cargo not found — using prebuilt binary (macOS Apple Silicon)"
  cp "$SELF_DIR/target/release/memnir" "$BIN"
else
  echo "  ERROR: no cargo and no usable prebuilt binary for $OS." >&2
  echo "  Install Rust (https://rustup.rs) and re-run — memnir compiles natively on WSL2/Linux." >&2
  exit 1
fi
chmod +x "$BIN"

echo "→ shell aliases (memnir, mn) → $SHELL_RC"
grep -q 'alias memnir=' "$SHELL_RC" 2>/dev/null || echo 'alias memnir="$HOME/.local/bin/memnir"' >> "$SHELL_RC"
grep -q 'alias mn=' "$SHELL_RC" 2>/dev/null || echo 'alias mn="$HOME/.local/bin/memnir"' >> "$SHELL_RC"

echo "→ peer config (~/.claude/memnir.conf)"
CONF="$HOME/.claude/memnir.conf"
if [ ! -s "$CONF" ]; then
  if [ -n "${MEMNIR_PEER:-}" ]; then
    echo "$MEMNIR_PEER" > "$CONF"; echo "  peer = $MEMNIR_PEER"
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
def has(ev): return any("memnir" in hk.get("command","") for g in h.get(ev,[]) for hk in g.get("hooks",[]))
if not has("SessionStart"):
    h.setdefault("SessionStart",[]).append({"hooks":[{"type":"command","command":f"{b} start >> {log} 2>&1"}]})
if not has("Stop"):
    h.setdefault("Stop",[]).append({"hooks":[{"type":"command","command":f"nohup {b} push >> {log} 2>&1 &"}]})
json.dump(d,open(p,"w"),indent=2,ensure_ascii=False); print("  hooks ok")
PY

echo ""
echo "✓ Memnir installed on $(hostname -s) — try: memnir doctor"
echo "  peer is read from ~/.claude/memnir.conf (or env MEMNIR_PEER)"
echo "  reload your shell:  source $SHELL_RC"
if [ "$IS_WSL" = 1 ]; then
  echo ""
  echo "  WSL2 note: memnir manages ~/.claude/memnir inside this distro."
  echo "  Run Claude Code *inside WSL2* so it shares the same ~/.claude."
  echo "  The Windows desktop app keeps memory on C:\\Users\\<you>\\.claude and is NOT bridged."
fi
