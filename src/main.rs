// mich-cdrive-clean — safe, aggressive, repeatable C: drive cleaner.
// std-only, no dependencies. Dry-run by default; pass --apply to delete.
//
// Safety model:
//   * Only explicit rule targets are ever touched; nothing else is walked.
//   * A global PROTECTED prefix list is checked before EVERY deletion
//     (Windows\Installer, Package Cache, WinSxS, DriverStore, live session
//     dirs, IMDb dataset cache, this tool's own folder, ...).
//   * Files locked by running processes are skipped, never forced.
//   * Age gates: some rules only delete files older than N days.

use std::env;
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------- win ffi --

#[link(name = "kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const u16,
        lpFreeBytesAvailableToCaller: *mut i64,
        lpTotalNumberOfBytes: *mut i64,
        lpTotalNumberOfFreeBytes: *mut i64,
    ) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn SHEmptyRecycleBinW(hwnd: *mut c_void, pszRootPath: *const u16, dwFlags: u32) -> i32;
    fn IsUserAnAdmin() -> i32;
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn disk_free_bytes(root: &str) -> Option<(i64, i64)> {
    let w = to_wide(root);
    let mut avail: i64 = 0;
    let mut total: i64 = 0;
    let mut free: i64 = 0;
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
    if ok != 0 { Some((free, total)) } else { None }
}

fn is_admin() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

fn empty_recycle_bin_c() -> bool {
    let w = to_wide("C:\\");
    // SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND
    let r = unsafe { SHEmptyRecycleBinW(std::ptr::null_mut(), w.as_ptr(), 0x1 | 0x2 | 0x4) };
    r == 0 // S_OK
}

// ---------------------------------------------------------------- helpers --

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn lc(s: &str) -> String {
    s.to_lowercase()
}

fn path_lc(p: &Path) -> String {
    lc(&p.to_string_lossy())
}

/// Case-insensitive fnmatch-style wildcard match (* and ?).
fn wild_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    fn m(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => (0..=t.len()).any(|i| m(&p[1..], &t[i..])),
            '?' => !t.is_empty() && m(&p[1..], &t[1..]),
            c => !t.is_empty() && t[0] == c && m(&p[1..], &t[1..]),
        }
    }
    m(&p, &t)
}

fn gb(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn envp(name: &str) -> PathBuf {
    env::var_os(name).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(""))
}

// ------------------------------------------------------------------ model --

struct Entry {
    path: PathBuf,
    size: u64,
    mtime: u64,
}

#[derive(Default)]
struct Stats {
    matched: u64, // bytes eligible for deletion
    freed: u64,   // bytes deleted (apply) or eligible (dry)
    young: u64,   // bytes skipped by age gate
    prot: u64,    // bytes skipped by protection
    locked: u64,  // bytes skipped (in use / access denied)
    n_freed: u64,
    n_locked: u64,
}

struct Ctx {
    apply: bool,
    now: u64,
    protected: Vec<String>, // lowercase prefixes never touched
    self_dir: String,
}

fn is_protected(ctx: &Ctx, p: &Path) -> bool {
    let s = path_lc(p);
    if !ctx.self_dir.is_empty() && s.starts_with(&ctx.self_dir) {
        return true;
    }
    ctx.protected.iter().any(|pre| s.starts_with(pre.as_str()))
}

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

fn file_meta(p: &Path) -> Option<fs::Metadata> {
    fs::symlink_metadata(p).ok()
}

fn mtime_of(md: &fs::Metadata) -> u64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_reparse(md: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Recursive walk collecting real files (never crosses reparse points/junctions).
fn walk(root: &Path, out: &mut Vec<Entry>, depth: u32) {
    if depth > 24 {
        return;
    }
    let rd = match fs::read_dir(root) {
        Ok(r) => r,
        Err(_) => return,
    };
    for ent in rd.flatten() {
        let p = ent.path();
        let md = match file_meta(&p) {
            Some(m) => m,
            None => continue,
        };
        if is_reparse(&md) {
            continue; // junction/symlink — never follow
        }
        if md.is_dir() {
            walk(&p, out, depth + 1);
        } else {
            out.push(Entry { path: p, size: md.len(), mtime: mtime_of(&md) });
        }
    }
}

fn collect(root: &Path) -> Vec<Entry> {
    let mut v = Vec::new();
    if root.exists() {
        walk(root, &mut v, 0);
    }
    v
}

type Guard<'a> = &'a dyn Fn(&Path) -> bool;

/// Try to delete one file according to gates. Never forces locked files.
fn delete_file(ctx: &Ctx, e: &Entry, min_age_days: u64, st: &mut Stats, guard: Option<Guard>) {
    if is_protected(ctx, &e.path) {
        st.prot += e.size;
        return;
    }
    if let Some(g) = guard {
        if g(&e.path) {
            st.prot += e.size;
            return;
        }
    }
    if min_age_days > 0 && ctx.now.saturating_sub(e.mtime) < min_age_days * 86400 {
        st.young += e.size;
        return;
    }
    st.matched += e.size;
    if !ctx.apply {
        st.freed += e.size;
        st.n_freed += 1;
        return;
    }
    match fs::remove_file(&e.path) {
        Ok(()) => {
            st.freed += e.size;
            st.n_freed += 1;
        }
        Err(_) => {
            st.locked += e.size;
            st.n_locked += 1;
        }
    }
}

/// Post-order removal of now-empty dirs under root (root itself last, if empty).
fn prune_dirs(ctx: &Ctx, root: &Path) {
    if !ctx.apply || !root.exists() {
        return;
    }
    fn rec(ctx: &Ctx, dir: &Path, root: &Path) {
        if let Ok(rd) = fs::read_dir(dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if let Some(md) = file_meta(&p) {
                    if md.is_dir() && !is_reparse(&md) {
                        rec(ctx, &p, root);
                    }
                }
            }
        }
        if let Ok(mut it) = fs::read_dir(dir) {
            if it.next().is_none() && !is_protected(ctx, dir) && dir != root {
                let _ = fs::remove_dir(dir);
            }
        }
    }
    rec(ctx, root, root);
    if let Ok(mut it) = fs::read_dir(root) {
        if it.next().is_none() && !is_protected(ctx, root) {
            let _ = fs::remove_dir(root);
        }
    }
}

// ------------------------------------------------------------------ rules --

struct Rule {
    name: String,
    desc: &'static str,
    run: Box<dyn Fn(&Ctx, &mut Stats)>,
}

fn tree_rule(name: &str, desc: &'static str, root: PathBuf, min_age_days: u64) -> Rule {
    Rule {
        name: name.to_string(),
        desc,
        run: Box::new(move |ctx, st| {
            if !root.exists() {
                return;
            }
            for e in collect(&root) {
                delete_file(ctx, &e, min_age_days, st, None);
            }
            prune_dirs(ctx, &root);
        }),
    }
}

fn files_rule(name: &str, desc: &'static str, root: PathBuf, pattern: &'static str, min_age_days: u64) -> Rule {
    Rule {
        name: name.to_string(),
        desc,
        run: Box::new(move |ctx, st| {
            if !root.exists() {
                return;
            }
            let mut hit = false;
            for e in collect(&root) {
                let fname = e.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if wild_match(pattern, &fname) {
                    hit = true;
                    delete_file(ctx, &e, min_age_days, st, None);
                }
            }
            if hit {
                prune_dirs(ctx, &root);
            }
        }),
    }
}

/// Delete directories whose NAME matches pattern, found anywhere under root (bounded depth).
fn name_dirs_rule(name: &str, desc: &'static str, root: PathBuf, pattern: &'static str, min_age_days: u64) -> Rule {
    Rule {
        name: name.to_string(),
        desc,
        run: Box::new(move |ctx, st| {
            if !root.exists() {
                return;
            }
            let mut targets: Vec<PathBuf> = Vec::new();
            fn find(root: &Path, pat: &str, out: &mut Vec<PathBuf>, depth: u32) {
                if depth > 10 {
                    return;
                }
                if let Ok(rd) = fs::read_dir(root) {
                    for ent in rd.flatten() {
                        let p = ent.path();
                        if let Some(md) = file_meta(&p) {
                            if is_reparse(&md) {
                                continue;
                            }
                            if md.is_dir() {
                                let n = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                                if wild_match(pat, &n) {
                                    out.push(p.clone());
                                }
                                find(&p, pat, out, depth + 1);
                            }
                        }
                    }
                }
            }
            find(&root, pattern, &mut targets, 0);
            for t in targets {
                for e in collect(&t) {
                    delete_file(ctx, &e, min_age_days, st, None);
                }
                prune_dirs(ctx, &t);
            }
        }),
    }
}

/// Keep the N newest dirs under root matching prefix; delete the rest.
fn keep_newest_dirs(name: &str, desc: &'static str, root: PathBuf, prefix: &str, keep: usize) -> Rule {
    let prefix = prefix.to_string();
    Rule {
        name: name.to_string(),
        desc,
        run: Box::new(move |ctx, st| {
            let mut items: Vec<(PathBuf, u64, String)> = Vec::new();
            if let Ok(rd) = fs::read_dir(&root) {
                for ent in rd.flatten() {
                    let p = ent.path();
                    let n = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    if !prefix.is_empty() && !n.starts_with(prefix.as_str()) {
                        continue;
                    }
                    if let Some(md) = file_meta(&p) {
                        if !md.is_dir() {
                            continue;
                        }
                        items.push((p, mtime_of(&md), n));
                    }
                }
            }
            if items.len() <= keep {
                return;
            }
            items.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2))); // newest first
            for (p, _, _) in items.iter().skip(keep) {
                for e in collect(p) {
                    delete_file(ctx, &e, 0, st, None);
                }
                prune_dirs(ctx, p);
            }
        }),
    }
}

/// Keep the N newest files matching pattern under root (recursive); delete the rest.
fn keep_newest_files(name: &str, desc: &'static str, root: PathBuf, pattern: &'static str, keep: usize) -> Rule {
    Rule {
        name: name.to_string(),
        desc,
        run: Box::new(move |ctx, st| {
            if !root.exists() {
                return;
            }
            let mut items: Vec<(PathBuf, u64)> = collect(&root)
                .into_iter()
                .filter(|e| {
                    let n = e.path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    wild_match(pattern, &n)
                })
                .map(|e| (e.path, e.mtime))
                .collect();
            if items.len() <= keep {
                return;
            }
            items.sort_by(|a, b| a.1.cmp(&b.1)); // oldest first
            let skipn = items.len() - keep;
            for (p, _) in items.iter().take(skipn) {
                let size = file_meta(p).map(|m| m.len()).unwrap_or(0);
                let e = Entry { path: p.clone(), size, mtime: 0 };
                delete_file(ctx, &e, 0, st, None);
            }
        }),
    }
}

fn exact_file_rule(name: &str, desc: &'static str, path: PathBuf) -> Rule {
    Rule {
        name: name.to_string(),
        desc,
        run: Box::new(move |ctx, st| {
            if let Some(md) = file_meta(&path) {
                if md.is_file() {
                    let e = Entry { path: path.clone(), size: md.len(), mtime: 0 };
                    delete_file(ctx, &e, 0, st, None);
                }
            }
        }),
    }
}

// ------------------------------------------------------------------- main --

fn print_help() {
    println!(
        "mich-cdrive-clean v1.0.0 — safe, aggressive, repeatable C: drive cleaner\n\
         \n\
         USAGE:\n\
         \x20 mich-cdrive-clean                    dry run (reports what WOULD be freed — default, always safe)\n\
         \x20 mich-cdrive-clean --apply            actually delete everything eligible\n\
         \x20 mich-cdrive-clean --apply --deep     also run DISM StartComponentCleanup (WinSxS, 5-20 min)\n\
         \x20 mich-cdrive-clean --apply --imdb     also purge C:\\Temp\\imdb-datasets + s22 (BlackCinema re-downloads them)\n\
         \x20 mich-cdrive-clean --apply --recycle  also empty the C: Recycle Bin\n\
         \x20 mich-cdrive-clean --apply --apps     also purge regenerable app data (Zoom ASR model)\n\
         \x20 mich-cdrive-clean --tips             only show big optional wins (needing YOUR decision)\n\
         \x20 mich-cdrive-clean --help             this help\n\
         \n\
         SAFETY: locked files are skipped, never forced. Windows\\Installer, Package Cache,\n\
         WinSxS, DriverStore, live session dirs and data caches are hard-protected.\n\
         Safe to run as often as you like; it converges (second run frees ~nothing).\n"
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let apply = args.iter().any(|a| a == "--apply");
    let deep = args.iter().any(|a| a == "--deep");
    let imdb = args.iter().any(|a| a == "--imdb");
    let recycle = args.iter().any(|a| a == "--recycle");
    let appsx = args.iter().any(|a| a == "--apps");
    let tips_only = args.iter().any(|a| a == "--tips");
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }

    let user = envp("USERPROFILE");
    let local = envp("LOCALAPPDATA");
    let roaming = envp("APPDATA");
    let pd = envp("ProgramData");
    let windir = envp("WINDIR");

    let self_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| path_lc(d)))
        .unwrap_or_default();

    // Global protection list — lowercase prefixes, checked before EVERY deletion.
    let mut protected: Vec<String> = vec![
        lc("c:\\windows\\installer"),
        lc("c:\\programdata\\package cache"),
        lc("c:\\windows\\winsxs"),
        lc("c:\\windows\\system32\\driverstore"),
        lc("c:\\programdata\\microsoft\\windows\\containers"),
        lc("c:\\temp\\startup-cleanup"),
        lc("c:\\temp\\zcode-"),
        lc("c:\\temp\\mission_tests"),
        lc("c:\\windows\\system32\\catroot2\\"), // live catroot2; only .bak-* copies are removed
    ];
    if !imdb {
        protected.push(lc("c:\\temp\\imdb-datasets"));
        protected.push(lc("c:\\temp\\s22"));
    }

    let ctx = Ctx { apply, now: now_secs(), protected, self_dir };

    if !is_admin() {
        println!("! NOTE: not elevated — system-level rules will partially skip (locked).");
    }

    let mut rules: Vec<Rule> = Vec::new();

    // ---- machine temp & scratch (with root-level *.ps1 guard for C:\Temp) ----
    {
        let temp_root = PathBuf::from("C:\\Temp");
        let temp_root2 = temp_root.clone();
        rules.push(Rule {
            name: "temp-c-temp".into(),
            desc: "C:\\Temp scratch sweep (session dirs, *.ps1, imdb cache protected)",
            run: Box::new(move |ctx, st| {
                if !temp_root2.exists() {
                    return;
                }
                let guard = |p: &Path| -> bool {
                    // keep small scripts sitting directly in C:\Temp root
                    if p.parent() == Some(Path::new("C:\\Temp")) {
                        return p.extension().map(|e| e.to_string_lossy().eq_ignore_ascii_case("ps1")).unwrap_or(false);
                    }
                    false
                };
                for e in collect(&temp_root2) {
                    delete_file(ctx, &e, 0, st, Some(&guard));
                }
                prune_dirs(ctx, &temp_root2);
            }),
        });
    }
    rules.push(tree_rule("temp-windows", "C:\\Windows\\Temp", windir.join("Temp"), 1));
    rules.push(tree_rule("temp-user", "user %TEMP%", local.join("Temp"), 1));
    rules.push(tree_rule("wu-download", "Windows Update download cache", windir.join("SoftwareDistribution\\Download"), 0));
    rules.push(files_rule("win-cbs-logs", "CBS/DISM logs > 7d", windir.join("Logs"), "*.log", 7));
    rules.push(exact_file_rule("win-memory-dmp", "C:\\Windows\\MEMORY.DMP", windir.join("MEMORY.DMP")));
    rules.push(tree_rule("win-minidumps", "minidumps", windir.join("Minidump"), 0));
    rules.push(tree_rule("win-livekernel", "LiveKernelReports", windir.join("LiveKernelReports"), 0));
    rules.push(name_dirs_rule("win-catroot2-baks", "catroot2.bak-* repair backups", windir.join("System32"), "catroot2.bak-*", 0));
    rules.push(tree_rule(
        "win-delivery-opt",
        "Delivery Optimization cache",
        windir.join("ServiceProfiles\\NetworkService\\AppData\\Local\\Microsoft\\Windows\\DeliveryOptimization\\Cache"),
        0,
    ));
    rules.push(tree_rule("win-wer-machine", "WER reports (machine)", pd.join("Microsoft\\Windows\\WER\\ReportQueue"), 0));
    rules.push(tree_rule("win-wer-archive", "WER reports archive (machine)", pd.join("Microsoft\\Windows\\WER\\ReportArchive"), 0));
    rules.push(tree_rule("win-wer-user", "WER reports (user)", local.join("Microsoft\\Windows\\WER"), 0));

    // ---- user caches --------------------------------------------------------------
    rules.push(tree_rule("u-d3dscache", "DirectX shader cache", local.join("D3DSCache"), 0));
    rules.push(files_rule("u-crashdumps", "%LOCALAPPDATA%\\CrashDumps", local.join("CrashDumps"), "*.dmp", 0));
    rules.push(files_rule("u-thumbcache", "thumbnail caches", local.join("Microsoft\\Windows\\Explorer"), "thumbcache_*", 0));
    rules.push(files_rule("u-iconcache", "icon caches", local.join("Microsoft\\Windows\\Explorer"), "iconcache_*", 0));
    rules.push(tree_rule("u-nv-dxcache", "NVIDIA DXCache", local.join("NVIDIA\\DXCache"), 0));
    rules.push(tree_rule("u-nv-glcache", "NVIDIA GLCache", local.join("NVIDIA\\GLCache"), 0));
    rules.push(tree_rule("u-amd-dxcache", "AMD DxCache", local.join("AMD\\DxCache"), 0));
    rules.push(tree_rule("u-amd-dx9cache", "AMD Dx9Cache", local.join("AMD\\Dx9Cache"), 0));
    rules.push(tree_rule("u-amd-glcache", "AMD GLCache", local.join("AMD\\GLCache"), 0));
    rules.push(tree_rule("u-amd-vkcache", "AMD VkCache", local.join("AMD\\VkCache"), 0));
    let cache_roots: [(&str, PathBuf); 3] = [
        ("u-chrome", local.join("Google\\Chrome")),
        ("u-edge", local.join("Microsoft\\Edge\\User Data")),
        ("u-pwmcp", local.join("ms-playwright-mcp")),
    ];
    for (nm, root) in cache_roots {
        for cn in ["Cache", "Code Cache", "GPUCache", "Media Cache", "GrShaderCache", "ShaderCache"] {
            let n = format!("{}-{}", nm, lc(cn).replace(' ', ""));
            rules.push(name_dirs_rule(&n, "browser/app cache dirs (logins untouched)", root.clone(), cn, 0));
        }
    }
    rules.push(tree_rule("u-npm-cache", "npm cache", local.join("npm-cache"), 0));
    rules.push(tree_rule("u-pip-cache", "pip cache", local.join("pip\\cache"), 0));
    rules.push(tree_rule("u-uv-cache", "uv cache (hardlink-safe)", local.join("uv\\cache"), 0));

    // ---- stale app versions (keep newest) -----------------------------------------
    rules.push(keep_newest_dirs("a-claude-app", "AnthropicClaude: keep newest app-* only", local.join("AnthropicClaude"), "app-", 1));
    rules.push(keep_newest_files("a-claude-nupkg", "AnthropicClaude stale packages", local.join("AnthropicClaude\\packages"), "*.nupkg", 0));
    rules.push(files_rule("a-claude-oldexe", "claude.exe.old leftovers", local.join("AnthropicClaude"), "claude.exe.old", 0));
    rules.push(keep_newest_files("a-claude-cli", "claude CLI: keep newest version only", user.join(".local\\share\\claude\\versions"), "*", 1));
    {
        // Roaming claude-code\<old versions>: stale copies superseded by .local CLI install
        let local_versions = user.join(".local\\share\\claude\\versions");
        let roam_root = roaming.join("Claude\\claude-code");
        rules.push(Rule {
            name: "a-claude-roam".into(),
            desc: "Roaming claude-code old copies (superseded by .local CLI)",
            run: Box::new(move |ctx, st| {
                let newest_local = collect(&local_versions).into_iter().map(|e| e.mtime).max().unwrap_or(0);
                if newest_local == 0 {
                    return; // no .local install -> leave Roaming alone
                }
                let mut items: Vec<PathBuf> = Vec::new();
                if let Ok(rd) = fs::read_dir(&roam_root) {
                    for ent in rd.flatten() {
                        let p = ent.path();
                        if let Some(md) = file_meta(&p) {
                            if md.is_dir() && !is_reparse(&md) && mtime_of(&md) < newest_local {
                                items.push(p);
                            }
                        }
                    }
                }
                for p in items {
                    for e in collect(&p) {
                        delete_file(ctx, &e, 0, st, None);
                    }
                    prune_dirs(ctx, &p);
                }
            }),
        });
    }
    rules.push(tree_rule("a-zcode-updater", "ZCode updater cache", local.join("@zcodedesktop-updater"), 0));
    rules.push(tree_rule("a-zcode-crash", "ZCode crash archives", user.join(".zcode\\v2\\crash"), 0));
    rules.push(files_rule("a-zcode-logs", "ZCode CLI logs > 7d", user.join(".zcode\\cli\\log"), "*.jsonl", 7));
    rules.push(files_rule("a-ultai-sessions", "ultimate-ai sessions > 7d", user.join(".ultimate-ai\\sessions"), "*.json", 7));
    rules.push(tree_rule("a-drweb", "Dr.Web leftover log folder", user.join("Doctor Web"), 0));
    rules.push(tree_rule("a-gnet-downloads", "Gnet installer downloads", user.join("Documents\\WindowsPowerShell\\Gnet\\Reports\\downloads"), 0));
    rules.push(exact_file_rule("a-qb-pdb", "qBittorrent debug symbols", PathBuf::from("C:\\Program Files\\qBittorrent\\qbittorrent.pdb")));
    rules.push(files_rule("a-macrium-testvhd", "Macrium test vhdx", pd.join("Macrium\\CurrentWindowsMigration"), "*.vhdx", 0));
    rules.push(tree_rule("a-gameautoinstall", "GameAutoInstall inno temp", local.join("Codex\\GameAutoInstall\\state\\inno-temp"), 0));
    rules.push(files_rule("a-gameautoinstall-logs", "GameAutoInstall logs > 7d", local.join("Codex\\GameAutoInstall\\state"), "*.log", 7));
    rules.push(tree_rule("a-laya-setup", "LayaAutoSetup installer", local.join("LayaAutoSetup"), 0));
    rules.push(tree_rule("a-gcc-downloads", "GIGABYTE Control Center download cache", PathBuf::from("C:\\Program Files\\GIGABYTE\\Control Center\\Lib\\Download"), 0));
    rules.push(tree_rule("a-dlled-installs", "Downloaded Installations cache", local.join("Downloaded Installations"), 0));

    // ---- v1.1.0 additions: machine-specific verified junk --------------------------
    {
        // game-library test fixtures dumped in the home dir (test-data-<timestamp>)
        let home = user.clone();
        rules.push(Rule {
            name: "a-testdata-home".into(),
            desc: "test-data-* fixture dirs in user home",
            run: Box::new(move |ctx, st| {
                if let Ok(rd) = fs::read_dir(&home) {
                    for ent in rd.flatten() {
                        let n = ent.file_name().to_string_lossy().to_string();
                        if n.starts_with("test-data-") {
                            if let Some(md) = file_meta(&ent.path()) {
                                if md.is_dir() && !is_reparse(&md) {
                                    for e in collect(&ent.path()) {
                                        delete_file(ctx, &e, 0, st, None);
                                    }
                                    prune_dirs(ctx, &ent.path());
                                }
                            }
                        }
                    }
                }
            }),
        });
    }
    rules.push(tree_rule(
        "win-sandbox-layers",
        "Windows Sandbox disposable layers (auto-rebuild on next sandbox use)",
        pd.join("Microsoft\\Windows\\Containers\\Layers"),
        0,
    ));
    rules.push(Rule {
        name: "a-zcode-drafts".into(),
        desc: "zcode session drafts INACTIVE > 7 days (dir mtime)",
        run: Box::new({
            let user2 = user.clone();
            move |ctx, st| {
            let ws = user2.join(".zcode\\workspace");
            if let Ok(rd) = fs::read_dir(&ws) {
                for ent in rd.flatten() {
                    let n = ent.file_name().to_string_lossy().to_string();
                    if !n.starts_with("draft-") {
                        continue;
                    }
                    if let Some(md) = file_meta(&ent.path()) {
                        // only drafts whose own dir mtime is older than 7 days (= inactive;
                        // active sessions touch their workspace constantly)
                        if md.is_dir() && !is_reparse(&md) && mtime_of(&md) > 0
                            && ctx.now.saturating_sub(mtime_of(&md)) > 7 * 86400
                        {
                            for e in collect(&ent.path()) {
                                delete_file(ctx, &e, 0, st, None);
                            }
                            prune_dirs(ctx, &ent.path());
                        }
                    }
                }
            }
            }
        }),
    });
    rules.push(exact_file_rule("a-macrium-waik", "Macrium WAIK installer leftover", PathBuf::from("C:\\Program Files\\Macrium\\Reflect\\WAIKFiles15x64.exe")));

    // ---- optional flags -------------------------------------------------------------
    if imdb {
        rules.push(tree_rule("o-imdb-main", "IMDb gz dataset cache (re-downloads on next index build)", PathBuf::from("C:\\Temp\\imdb-datasets"), 0));
        rules.push(tree_rule("o-imdb-s22", "IMDb s22 stale snapshot", PathBuf::from("C:\\Temp\\s22"), 0));
    }
    if appsx {
        rules.push(tree_rule("o-zoom-asr", "Zoom ASR model (regenerates)", roaming.join("Zoom\\asr"), 0));
    }

    // ---- quarantine suspicious binaries ----------------------------------------------
    {
        let qsrc = PathBuf::from("C:\\ProgramData\\wildrig.exe");
        if qsrc.exists() {
            let quarantine_dir = PathBuf::from("C:\\Temp\\startup-cleanup\\quarantine");
            rules.push(Rule {
                name: "x-quarantine-miner".into(),
                desc: "QUARANTINE wildrig.exe (miner) -> startup-cleanup\\quarantine",
                run: Box::new(move |ctx, st| {
                    let sz = file_meta(&qsrc).map(|m| m.len()).unwrap_or(0);
                    if !ctx.apply {
                        st.matched += sz;
                        st.freed += sz;
                        return;
                    }
                    let _ = fs::create_dir_all(&quarantine_dir);
                    let dest = quarantine_dir.join("wildrig.exe");
                    match fs::rename(&qsrc, &dest) {
                        Ok(()) => {
                            st.matched += sz;
                            st.freed += sz;
                            st.n_freed += 1;
                        }
                        Err(_) => {
                            st.locked += sz;
                            st.n_locked += 1;
                        }
                    }
                }),
            });
        }
    }

    // ---- tips ----------------------------------------------------------------------
    let tips: Vec<(&str, PathBuf)> = vec![
        ("Docker Desktop app (data already lives on F:; uninstall app if unused)", PathBuf::from("C:\\Program Files\\Docker")),
        ("WSL Ubuntu disk C:\\wsl2 (protected; compact via wsl --manage)", PathBuf::from("C:\\wsl2")),
        ("Python 3.12 site-packages (if 3.12 retired)", local.join("Programs\\Python\\Python312\\Lib\\site-packages")),
        (".laya venv (if laya inactive)", user.join(".laya\\venv")),
    ];

    if tips_only {
        println!("=== BIG OPTIONAL WINS (need your decision — NOT deleted by this tool) ===");
        for (label, p) in &tips {
            let sz: u64 = collect(p).iter().map(|e| e.size).sum();
            if sz > 0 {
                println!("  {:>10}  {}  [{}]", gb(sz), label, p.display());
            }
        }
        println!("\nAlso consider: cap pagefile (94 GB RAM), powercfg /h /type reduced.");
        return;
    }

    let (free0, total) = disk_free_bytes("C:\\").unwrap_or((0, 0));
    println!(
        "{}\nC: free before: {} of {}\n",
        if apply { "=== APPLY MODE — deleting eligible files ===" } else { "=== DRY RUN — nothing deleted (pass --apply) ===" },
        gb(free0 as u64),
        gb(total as u64)
    );

    let mut grand = Stats::default();
    println!("{:<30} {:>12} {:>12} {:>12} {:>8}", "RULE", "ELIGIBLE", "FREED", "SKIPPED", "FILES");
    for r in &rules {
        let mut st = Stats::default();
        (r.run)(&ctx, &mut st);
        let skipped = st.young + st.prot + st.locked;
        if st.matched > 0 || st.locked > 0 || st.young > 0 || st.prot > 0 {
            println!(
                "{:<30} {:>12} {:>12} {:>12} {:>8}  {}",
                r.name, gb(st.matched), gb(st.freed), gb(skipped), st.n_freed, r.desc
            );
        }
        grand.matched += st.matched;
        grand.freed += st.freed;
        grand.young += st.young;
        grand.prot += st.prot;
        grand.locked += st.locked;
        grand.n_freed += st.n_freed;
        grand.n_locked += st.n_locked;
    }
    println!("{}", "-".repeat(110));
    println!(
        "TOTAL: eligible {} | {} {} | skipped: age {} / protected {} / locked {}\n",
        gb(grand.matched),
        if apply { "FREED" } else { "WOULD FREE" },
        gb(grand.freed),
        gb(grand.young),
        gb(grand.prot),
        gb(grand.locked)
    );

    if recycle && apply {
        if empty_recycle_bin_c() {
            println!("Recycle Bin (C:) emptied.");
        } else {
            println!("Recycle Bin (C:) skipped or already empty.");
        }
    }

    if deep && apply {
        println!("\n--deep: running DISM StartComponentCleanup (this can take 5-20 min)...");
        let st = Command::new("dism.exe")
            .args(["/Online", "/Cleanup-Image", "/StartComponentCleanup", "/NoRestart"])
            .status();
        match st {
            Ok(s) => println!("DISM exit: {}", s.code().unwrap_or(-1)),
            Err(e) => println!("DISM failed to launch: {e}"),
        }
    }

    let (free1, _) = disk_free_bytes("C:\\").unwrap_or((0, 0));
    if apply {
        println!("C: free after : {}  (delta this run: {})", gb(free1 as u64), gb(free1.saturating_sub(free0) as u64));
    }

    println!("\n=== BIG OPTIONAL WINS (need your decision — NOT deleted) ===");
    for (label, p) in &tips {
        let sz: u64 = collect(p).iter().map(|e| e.size).sum();
        if sz > 1024 * 1024 {
            println!("  {:>10}  {}", gb(sz), label);
        }
    }
    println!("\nAlso consider: cap pagefile (94 GB RAM), powercfg /h /type reduced, Docker/WSL/GIGABYTE-AI uninstalls.");
}

// ------------------------------------------------------------------ tests --

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_basics() {
        assert!(wild_match("*.log", "CBS.log"));
        assert!(wild_match("*.log", "cbs.LOG")); // case-insensitive
        assert!(!wild_match("*.log", "CBS.etl"));
        assert!(wild_match("app-*", "app-4.16336.0"));
        assert!(!wild_match("app-*", "packages"));
        assert!(wild_match("catroot2.bak-*", "catroot2.bak-20261101-171220"));
        assert!(!wild_match("catroot2.bak-*", "catroot2"));
        assert!(wild_match("a?c", "abc"));
        assert!(!wild_match("a?c", "abbc"));
        assert!(wild_match("*", "anything.at.all"));
        assert!(wild_match("claude.exe.old", "claude.exe.OLD"));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(gb(0), "0 B");
        assert_eq!(gb(2048), "2.0 KB");
        assert_eq!(gb(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(gb(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn protection_prefixes_block_deletion() {
        let ctx = Ctx {
            apply: true,
            now: 0,
            protected: vec![
                lc("c:\\windows\\installer"),
                lc("c:\\programdata\\package cache"),
                lc("c:\\windows\\winsxs"),
                lc("c:\\temp\\imdb-datasets"),
            ],
            self_dir: lc("f:\\backup\\rust\\target"),
        };
        assert!(is_protected(&ctx, Path::new("C:\\Windows\\Installer\\7f07.msi")));
        assert!(is_protected(&ctx, Path::new("C:\\ProgramData\\Package Cache\\{GUID}\\x.msi")));
        assert!(is_protected(&ctx, Path::new("C:\\WINDOWS\\WinSxS\\amd64_foo\\msedge.dll")));
        assert!(is_protected(&ctx, Path::new("C:\\Temp\\imdb-datasets\\title.akas.tsv.gz")));
        assert!(is_protected(&ctx, Path::new("F:\\backup\\rust\\target\\x86_64-pc-windows-msvc\\release\\mich-cdrive-clean.exe")));
        assert!(!is_protected(&ctx, Path::new("C:\\Temp\\WPR_initiated_x.etl")));
        assert!(!is_protected(&ctx, Path::new("C:\\Windows\\System32\\catroot2.bak-20261101-171220\\catdb")));
        assert!(!is_protected(&ctx, Path::new("C:\\Windows\\System32\\catroot2\\{F750E6C3}\\catdb"))); // live dir OK to *see*; rules never target it
    }

    #[test]
    fn age_gate_arithmetic() {
        let now = 1_800_000_000u64;
        let day = 86_400u64;
        // entry 10 days old, rule requires 7 -> eligible
        assert!(now.saturating_sub(now - 10 * day) >= 7 * day);
        // entry 3 days old, rule requires 7 -> skipped
        assert!(now.saturating_sub(now - 3 * day) < 7 * day);
    }
}
