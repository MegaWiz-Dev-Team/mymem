# Changelog

All notable changes to Memnir are documented here. Format follows [Keep a Changelog](https://keepachangelog.com); versioning is [SemVer](https://semver.org).

## [Unreleased]

### Added
- **WSL2 install support** — `install.sh` detects WSL2 (`/proc/version` + `WSL_DISTRO_NAME`), wires the alias into the actual login shell rc (`~/.bashrc` on WSL2, `~/.zshrc` on macOS), and warns when `~/.claude` is absent but a Windows-side `/mnt/c/Users/*/.claude` exists (hooks would otherwise land in the wrong `settings.json`). (#1, #3)
- **Installer preflight** — validates `rsync`/`python3`/cargo-or-prebuilt before touching the system, with a C-linker (`cc`) check on Linux and an apt one-liner on failure; on any miss it exits with "nothing was changed". `tailscale`/`ssh` are reported as warnings. (#2, #3)
- **Release CI** (`.github/workflows/release.yml`) — builds prebuilt binaries on every `v*` tag for `aarch64-apple-darwin`, `x86_64-apple-darwin`, and `x86_64-unknown-linux-gnu` (WSL2), with SHA-256 sidecars. Native Windows is intentionally excluded until `src/main.rs` drops the unconditional Unix `symlink`. (#3)
- README **Linux / WSL2** section (EN + TH) and updated requirements.

## [0.2.0] — 2026-06-06

### Added
- **`compact-index` (Tier-0 split)** — caps the always-on `MEMORY.md` token footprint. Keeps only Tier-0 types (default `user,feedback`; configurable) in the auto-loaded index and spills the full catalog to `MEMORY.full.md` — not auto-loaded, but still on disk and fully searchable (`search` scans the pool, not the index). Persisted via a `.index_compact` marker so `sync`/`pull`/`share` keep regenerating the compact form. Reversible with `--off`; no data loss (the index is always regenerated from the memory files). `MEMORY.full.md` is local-only (never synced).
- **`fix-links [--apply]`** — repairs broken `[[links]]` that have exactly one unambiguous normalized-substring target. Dry-run by default; conservative — typos and deliberate forward-references are reported but left untouched. Makes the `doctor` "→ memnir fix-links" action a real command.
- `autolink` documented in `help` and the README command table (already existed in dispatch).

### Changed
- `doctor`'s index action now points at the real `memnir compact-index` command (was an aspirational label).
- `regen_index` is tier-aware and preserves hand-curated lines from both `MEMORY.md` and `MEMORY.full.md`; the loader now excludes all `MEMORY*.md` files from the pool.
- 4 new unit tests (18 total), clippy clean.

## [0.1.1] — 2026-06-05

### Added
- **Multi-machine mesh** — `~/.claude/memnir.conf` lists every other machine (one `user@host` per line; `MEMNIR_PEER` comma-separated). `push`/`pull` fan out to all peers. `doctor` shows per-peer reachability.
- **Origin tracking** — each memory carries `metadata.origin: <hostname>` (machine that first wrote it), stamped before push; pre-existing memories grandfathered as `?` via a per-machine `.origin_baseline` (non-destructive). Surfaced in `status`, `doctor`, `list`, and a dashboard **Origins** panel + node tooltip.
- Short `mn` shell alias alongside `memnir`.

### Changed
- Refactored into pure, unit-tested helpers (13 tests, clippy clean); dashboard HTML extracted to `dashboard.template.html` via `include_str!`; `share`/`local`/`toggle` unified behind `apply_scope`.

## [0.1.0] — 2026-06-05

Initial public release. A single Rust binary (pure std, no external crates) that shares Claude Code memory across machines and sessions over Tailscale.

### Added
- **Scope-filtered sync** — only memories tagged `metadata.scope: shared` cross machines; everything else stays local. Two-way `rsync -auz` (newest-wins, never deletes); `MEMORY.md` regenerated per-machine so local titles never leak.
- **Commands** — `sync`, `push`, `pull`, `start`, `share <id>`, `local <id>`, `list`, `link`, `status`, `doctor`, `dash`, `serve`, `help`.
- **`doctor`** — health report: token footprint (always-on index vs pool), broken `[[links]]` (with `-`/`_` normalization), oversized memories, isolated nodes, scope suggestions; `--check` mode for hooks.
- **`dash`** — static `dashboard.html`: knowledge-graph (vis-network) + token visualization, white / mint-green theme, Mímir's-Well logo.
- **`serve`** — interactive dashboard on `127.0.0.1` (localhost + random per-session token): click a node to toggle shared/local, Sync / Refresh buttons, Commands panel with where-to-use tags.
- **Auto-sync hooks** — `SessionStart` runs `start` (autolink current project + sync), `Stop` pushes new memory; installed by `install.sh`.
- **Per-machine peer config** — read from `~/.claude/memnir.conf` or env `MEMNIR_PEER`; no host data baked into the binary.
- `install.sh` bootstrapper, English + Thai READMEs, MIT license.

[0.2.0]: https://github.com/MegaWiz-Dev-Team/memnir/releases/tag/v0.2.0
[0.1.1]: https://github.com/MegaWiz-Dev-Team/memnir/releases/tag/v0.1.1
[0.1.0]: https://github.com/MegaWiz-Dev-Team/memnir/releases/tag/v0.1.0
