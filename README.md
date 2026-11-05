# mich-cdrive-clean

![Rust](https://img.shields.io/badge/Rust-stable-orange) ![Platform](https://img.shields.io/badge/Platform-Windows-blue) ![License](https://img.shields.io/badge/License-MIT-green) ![Dependencies](https://img.shields.io/badge/Dependencies-0_(std_only)-brightgreen)

**A safe, aggressive, repeatable C: drive cleaner for Windows — written in pure Rust with zero dependencies.**
Dry-run by default, `--apply` to clean, converges to zero on the second run, and never touches
anything you actually need.

On its first real run on the author's machine it freed **~13.7 GB** (junk temp, stale Electron/CLI
versions, crash dumps, browser/app caches, telemetry traces, a quarantined miner) with every
login, profile, dataset and project left intact.

---

## Why

Windows accumulates layers of *provably-dead* weight: WPR trace files nobody reads, five copies
of an Electron app from three versions ago, update caches, debug symbol files, shader caches,
quarantine-worthy binaries in `ProgramData`. Generic cleaners either stop at the safe stuff
(Disk Cleanup) or are reckless with your data (random "boost" tools). This one is opinionated:

- **Every rule is explicit.** Nothing is walked that isn't named by a rule.
- **A hard protection list is checked before every single deletion** — `Windows\Installer`,
  `Package Cache`, `WinSxS`, `DriverStore`, live session dirs, data caches (IMDb dataset cache),
  and the tool's own folder.
- **Locked files are skipped, never forced.** If a running app holds a file, it stays — run the
  tool again after closing the app.
- **Age gates.** Logs and session files are only removed when older than 7 days; fresh temp is
  left alone for a day so running installers never break.
- **Newest version always kept.** Electron `app-*` dirs, CLI version dirs and update packages
  are pruned to the newest one *by modification time* — never by guessed version numbers.

## What it cleans (rule categories)

| Category | Examples |
|---|---|
| Machine temp & scratch | `C:\Temp` sweep (protects session dirs, root `*.ps1`, IMDb cache), `C:\Windows\Temp`, `%TEMP%` |
| Windows internals | WU download cache, CBS/DISM logs > 7d, MEMORY.DMP, minidumps, LiveKernelReports, `catroot2.bak-*` repair backups, Delivery Optimization cache, WER reports |
| GPU / browser / package caches | D3DSCache, NVIDIA DX/GLCache, AMD Dx/Dx9/GL/VkCache, Chrome/Edge **cache dirs only** (logins untouched), npm/pip/uv caches, playwright-mcp profile caches |
| Stale app versions | AnthropicClaude `app-*` + stale `.nupkg` + `claude.exe.old`, claude CLI versions, Roaming claude-code superseded copies, ZCode updater cache & crash dumps & old logs |
| Verified leftovers | Dr.Web log folder, Gnet downloaded installers, qBittorrent `.pdb`, Macrium test vhdx, GameAutoInstall inno-temp, LayaAutoSetup, GIGABYTE Control Center download cache, Downloaded Installations |
| Security | **Quarantines** (moves, not deletes) known-bad binaries — e.g. `C:\ProgramData\wildrig.exe` (cryptominer) → `C:\Temp\startup-cleanup\quarantine\` |

Optional flags: `--imdb` (also purge the IMDb dataset cache — BlackCinema re-downloads it),
`--apps` (Zoom ASR model), `--recycle` (empty C: Recycle Bin), `--deep` (DISM
`StartComponentCleanup` — Microsoft-supported WinSxS compaction, 5–20 min).

At the end of every run it prints **big optional wins** — multi-GB items that need a human
decision (Docker/WSL uninstall, unused Python torch stacks, pagefile cap) and are never deleted
automatically.

## Requirements

- Windows 10/11, x64
- No runtime dependencies — single static-ish exe (~330 KB)
- Elevated shell recommended (system rules skip silently otherwise)

## Install

Download **`mich-cdrive-clean.exe`** from the latest
[GitHub Release](https://github.com/Michaelunkai/mich-cdrive-clean/releases/latest) and run it
from any terminal (or double-click for a dry-run report).

## Usage

```text
mich-cdrive-clean                    dry run — full report, deletes NOTHING (default, always safe)
mich-cdrive-clean --apply            clean everything eligible
mich-cdrive-clean --apply --deep     + DISM StartComponentCleanup (WinSxS, 5-20 min)
mich-cdrive-clean --apply --recycle  + empty the C: Recycle Bin
mich-cdrive-clean --apply --imdb     + purge IMDb dataset cache (re-downloads on next index build)
mich-cdrive-clean --tips             just list big optional wins needing YOUR decision
```

Typical session:

```text
=== APPLY MODE — deleting eligible files ===
C: free before: 1711.14 GB of 1861.57 GB

RULE                               ELIGIBLE        FREED      SKIPPED    FILES
temp-c-temp                         6.68 GB       6.60 GB      1.17 GB    88671
a-claude-app                       869.4 MB      869.4 MB          0 B       96
a-gnet-downloads                   925.0 MB      925.0 MB          0 B       17
u-pwmcp-cache                      768.8 MB      768.8 MB          0 B     4048
...
TOTAL: eligible 13.50 GB | FREED 13.15 GB | skipped: age 133.8 MB / protected 1.17 GB / locked 356.2 MB
C: free after : 1723.85 GB  (delta this run: 12.70 GB)
```

Run it as often as you like — the second run frees ~nothing (convergence proof), and anything
still locked by a running app gets picked up next time.

## Safety model in depth

1. **Rule-scoped walking** — each rule names its root; there is no "scan everything" mode.
2. **Reparse points are never followed** — junctions into other volumes or system areas can't
   redirect a deletion.
3. **Per-file protection check** — the global protection list wins over every rule.
4. **No service kills, no forced unlocks, no reboots.**
5. **Quarantine instead of delete** for suspicious binaries (preserved as evidence).
6. **Keep-newest semantics** for anything versioned.

## Build from source

```text
git clone https://github.com/Michaelunkai/mich-cdrive-clean.git
cd mich-cdrive-clean
cargo build --release --target x86_64-pc-windows-msvc
cargo test
```

No crates.io dependencies — compiles offline in seconds.

## Troubleshooting

- **"not elevated" warning** → run from an administrator terminal; system rules otherwise skip.
- **Large "locked" totals** → files held by running apps (Claude, Chrome, NVIDIA). Close and re-run.
- **Recycle Bin not emptied** → pass `--recycle` (off by default, on purpose).
- **Want the IMDb dataset cache gone too** → `--apply --imdb` (the BlackCinema index task
  re-downloads ~1.2 GB on its next rebuild).

## License

MIT — see `src/main.rs` header. Provided as-is: always run the dry-run first and read the report.
