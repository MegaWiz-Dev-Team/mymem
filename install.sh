#!/bin/bash
# Memnir installer — build the Rust binary and wire it into Claude on this machine.
# Sets up: memnir binary (~/.local/bin), shell alias, project symlinks, auto-sync hooks.
set -euo pipefail

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
SM="$HOME/.claude/memnir"
BIN="$HOME/.local/bin/memnir"
LOG="$HOME/.claude/memnir.log"

echo "→ build memnir (Rust)"
mkdir -p "$SM" "$HOME/.claude/projects" "$HOME/.local/bin"
if command -v cargo >/dev/null 2>&1; then
  ( cd "$SELF_DIR" && cargo build --release )
  cp "$SELF_DIR/target/release/memnir" "$BIN"
elif [ -f "$SELF_DIR/target/release/memnir" ]; then
  echo "  cargo not found — using prebuilt binary (must be same arch: Apple Silicon)"
  cp "$SELF_DIR/target/release/memnir" "$BIN"
else
  echo "  ERROR: no cargo and no prebuilt binary. Install Rust (https://rustup.rs) or copy a built binary to $BIN" >&2
  exit 1
fi
chmod +x "$BIN"

echo "→ shell aliases (memnir, mn)"
grep -q 'alias memnir=' "$HOME/.zshrc" 2>/dev/null || echo 'alias memnir="$HOME/.local/bin/memnir"' >> "$HOME/.zshrc"
grep -q 'alias mn=' "$HOME/.zshrc" 2>/dev/null || echo 'alias mn="$HOME/.local/bin/memnir"' >> "$HOME/.zshrc"

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
