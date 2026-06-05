# Changelog

All notable changes to Memnir are documented here. Format follows [Keep a Changelog](https://keepachangelog.com); versioning is [SemVer](https://semver.org).

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

[0.1.1]: https://github.com/MegaWiz-Dev-Team/memnir/releases/tag/v0.1.1
[0.1.0]: https://github.com/MegaWiz-Dev-Team/memnir/releases/tag/v0.1.0
