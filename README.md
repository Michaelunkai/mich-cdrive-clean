# mich-cdrive-clean

![Rust](https://img.shields.io/badge/Rust-stable-orange) ![Platform](https://img.shields.io/badge/Platform-Windows-blue) ![License](https://img.shields.io/badge/License-MIT-green) ![Dependencies](https://img.shields.io/badge/Dependencies-0_(std_only)-brightgreen) ![Target](https://img.shields.io/badge/Free_space_target-1775_GB-ff69b4)

> **A safe, aggressive, repeatable C: drive cleaner for Windows — pure Rust, zero dependencies, one 386 KB exe.**
> Dry-run by default. `--apply` to clean. Converges to ~nothing on the second run.
> Ends every run by **verifying a hard free-space target** — and tells you exactly what stands between you and it if short.

On the author's heavily-loaded AI workstation it took C: from **1,711 GB free to 1,788 GB free** (77 GB reclaimed, verified across dozens of live runs) while a 500+ process fleet — 40+ Claude sessions, Docker, WSL distros, qBittorrent seeding 60+ torrents, game-install daemons — kept running the whole time. Nothing they use was touched.

---

## What makes it different

| Ordinary cleaners | mich-cdrive-clean |
|---|---|
| "Trust me" black-box scans | **60+ named rules** — nothing is touched that a rule doesn't name |
| One-shot buttons | **Convergent**: run it daily, it harvests whatever regrew (npm/temp caches, update leftovers, stale app versions) |
| Deletes what looks unused | **Locked files are never forced**; app-version dirs are deleted **all-or-nothing** (a dir with any held file is skipped whole — running installs can't be half-deleted) |
| Silent about the ceiling | Prints **`TARGET 1775 GB: HELD (…)`** — or the exact shortfall and the lever that closes it |
| Fake "boost" wins | Real fights: retires qBittorrent staging torrents via the daemon's own marker protocol, purges the 6 GB ACL-locked Delivery-Optimization cache via the supported API, holds the pagefile config against repair suites that keep rewriting it |

## The rule catalog

**Machine temp & OS internals** — `C:\Temp` scratch, `Windows\Temp`, `%TEMP%`, WU download cache, CBS/DISM logs (7d), CbsTemp servicing leftovers (1d), `SoftwareDistribution.old_*` repair leftovers (found 2.58 GB in the wild), `catroot2.bak-*`/`.old`, WDI boot ETLs, archived event logs, WER reports, MEMORY.DMP, minidumps, LiveKernelReports, repair-cycle `configfix` registry snapshots (14d), dead installers (`vc_redist`/`vstor_redist` bundles, AMD chipset extraction caches, Macrium `.BAK`).

**GPU / browser / package caches** — D3DSCache, NVIDIA DX/GLCache, AMD Dx/Dx9/GL/VkCache, Chrome/Edge **cache dirs only** (logins untouched), npm/pip/uv/cargo-registry caches, playwright-mcp profiles.

**Stale app versions (the "Claude" feature)** — AnthropicClaude `app-*` keep-newest + stale `.nupkg` + `claude.exe.old`; claude CLI `.local` versions keep-newest; **Roaming claude-code old versions compared by version string** (mtime lies — a re-extracted old build looks new) with the all-or-nothing in-use guard; Defender platforms keep-newest-2; duplicate gh CLI (stale user-scope copy); Kiri updater msis; ZCode updater caches/crashes/logs; IMDb dataset cache as a 2-hour elastic reserve.

**Recurring space wars, automated** —

- **`qb-c-sources`**: a zero-dependency HTTP client (raw `TcpStream`, no crates) talks to the local qBittorrent WebUI, finds any torrent staged into `C:\qb-moved-sources`, retires it with the GameAutoInstall daemon's own `user-deleted-torrents.txt` marker protocol, deletes torrent + files, sweeps leftovers. E:/F: media libraries are never touched; qB unreachable ⇒ never blind-deletes. (This daemon re-staged 15–50 GB repacks twice; the class is now extinct.)
- **Delivery Optimization**: the file walker is useless here (NetworkService ACLs) — the exe calls `Delete-DeliveryOptimizationCache` and caps the cache at 1 GB by policy. A 5.9 GB pile was found and killed on day one of this rule.
- **Pagefile guard**: asserts *both* `AutomaticManagedPagefile=false` *and* fixed `16384/65536` on every run — compiled, so hostile repair suites can't text-edit it back to 48–96 GB. (Root cause of three mysterious −16 GB drops: the machine's own `addph` profile function flips auto-managed on, under which Windows ignores any fixed config.)
- **`*.reloc-old` sweep**: leftovers of the byte-verified junction-relocation program (VS BuildTools, Windows Kits, Package Cache, dotnet, Python, gradle, Adoptium, WinLibs, rustup, npm, uv, omp, laya, codex, Reference Assemblies… all live on a second drive and work through junctions — zero deletions, zero functional loss).

## Safety model

1. Only rule-named targets are walked. 2. A global **PROTECTED prefix list** (`Windows\Installer`, `Package Cache`, `WinSxS`, `DriverStore`, live session dirs, the tool's own folder) is checked before **every** deletion. 3. Reparse points are never crossed — junction relocations can't be double-counted or broken. 4. Locked files are skipped, never forced — close the app, run again. 5. Age gates everywhere (2 h – 30 d per category) so nothing in-flight dies. 6. App-version dirs: **all-or-nothing** (`tree_in_use` guard). 7. Multi-GB optional wins (Docker app dir, WSL disks, unused venvs) are printed as *your* decision — never taken.

## Install & usage

Download **`mich-cdrive-clean.exe`** from the [latest release](https://github.com/Michaelunkai/mich-cdrive-clean/releases/latest) (~386 KB, static, no runtime).

```text
mich-cdrive-clean                    dry run — reports what WOULD be freed (default, always safe)
mich-cdrive-clean --apply            clean + assert pagefile/DO-cache configs + verify the 1775 GB target
mich-cdrive-clean --apply --deep     additionally DISM StartComponentCleanup + VSS cap (5-20 min)
mich-cdrive-clean --tips             only the big optional wins
```

Elevated shell recommended (system rules skip quietly otherwise). Run as often as you like — it converges.

## Campaign diary (all numbers real, 2026-11-05 → 11-08)

| Day | Event |
|---|---|
| 05 | Baseline **1,711 GB** → 1,720 held. Temp, stale Electron/CLI versions, caches, quarantined a cryptominer, 25 GB of dev tooling junctioned to F:. VSS capped. |
| 06 | Daemon re-staged *The Thaumaturge* (15.6 GB) — qb-c-sources rule born. Update-repair leftovers **+2.58 GB**. Live junction wave **+5.99 GB**. Held 1,775.64. First sabotage found: repair suite rewrote the pagefile config before every boot. |
| 07 | Watchdog deployed — and found **edited on disk** (constants swapped). Rewritten tamper-resistant (computed values). |
| 08 | Root cause: profile `addph` sets `AutomaticManagedPagefile` — flag cleared in binary + watchdog. **Delivery-Optimization 5.9 GB** purge + 1 GB cap. Old-version cleanup (claude-code 2.1.317 +322 MB, dup gh, dead installers). Reboot lands 16 GB pagefile. |
| End | **`TARGET 1775 GB: HELD (1,788.67 GB free)`** — margin 13.7 GB, converging runs harvest only megabytes of drift. Docker, WSL, Codex, 60+ torrents, every login and project: intact. |

## Build from source

```text
cargo build --release          # or: cargo test && cargo build --release
```

One file (`src/main.rs`), std-only: wide-string FFI (`GetDiskFreeSpaceExW`, `SHEmptyRecycleBinW`, `IsUserAnAdmin`), a hand-rolled HTTP/1.0 client for the qB WebUI, Hinnant's civil-date algorithm, and a tiny rules engine. 7 unit tests cover wildcards, size formatting, protection prefixes, age arithmetic, JSON parsing and the qB torrent scanner.

## License

MIT — see [LICENSE](LICENSE).
