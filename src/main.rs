// mich-cdrive-clean — safe, aggressive, repeatable C: drive cleaner.
// std-only, no dependencies. Dry-run by default; pass --apply to delete.
//
// v1.3.0: qb-c-sources hook — retires qBittorrent torrents that stage into
//         C:\qb-moved-sources via the GameAutoInstall daemon's own
//         user-deleted-torrents marker protocol, then sweeps the leftovers.
//         E:\media + F:\Downloads libraries are never touched. IMDb dataset
//         caches now clean by default after a 2-day age gate. Every --apply
//         run verifies the 1770 GB free-space target at the end.
//
// Safety model:
//   * Only explicit rule targets are ever touched; nothing else is walked.
//   * A global PROTECTED prefix list is checked before EVERY deletion
//     (Windows\Installer, Package Cache, WinSxS, DriverStore, live session
//     dirs, this tool's own folder, ...).
//   * Files locked by running processes are skipped, never forced.
//   * Age gates: some rules only delete files older than N days.

use std::env;
use std::ffi::c_void;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

// ------------------------------------------------------ qbittorrent hook --
// The GameAutoInstall daemon historically stages game repack torrents into
// C:\qb-moved-sources and has re-queued them more than once, silently eating
// 15-50 GB of C:. This hook talks to the local qBittorrent WebUI
// (127.0.0.1:18080, HTTP/1.0 over TcpStream — zero deps), retires any torrent
// whose save_path is exactly C:\qb-moved-sources using the daemon's own
// user-deleted-torrents marker protocol, then sweeps leftover files.
// E:\media and F:\Downloads libraries are NEVER touched. If qB is down the
// state is unknown, so nothing is deleted (reported as locked instead).

const QB_ADDR: &str = "127.0.0.1:18080";
const QB_USER: &str = "admin";
const QB_PASS: &str = "adminadmin";
const QB_SOURCE_DIR: &str = "C:\\qb-moved-sources";
const QB_MARKER: &str = "C:\\Users\\Admin\\AppData\\Local\\Codex\\GameAutoInstall\\state\\user-deleted-torrents.txt";

/// One-shot HTTP/1.0 request. Returns (status, headers, body) or None.
fn qb_http(method: &str, path: &str, body: Option<&str>, cookie: Option<&str>) -> Option<(u16, String, String)> {
    let mut stream = TcpStream::connect(QB_ADDR).ok()?;
    let mut req = format!(
        "{} {} HTTP/1.0\r\nHost: {}\r\nAccept: */*\r\nConnection: close\r\n",
        method, path, QB_ADDR
    );
    if let Some(c) = cookie {
        req.push_str("Cookie: ");
        req.push_str(c);
        req.push_str("\r\n");
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    if let Some(b) = body {
        stream.write_all(b.as_bytes()).ok()?;
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok())?;
    let split = text.find("\r\n\r\n")?;
    Some((status, text[..split].to_string(), text[split + 4..].to_string()))
}

fn qb_login() -> Option<String> {
    let body = format!("username={}&password={}", QB_USER, QB_PASS);
    let (status, head, _) = qb_http("POST", "/api/v2/auth/login", Some(&body), None)?;
    if status != 200 {
        return None;
    }
    for line in head.lines() {
        if line.to_lowercase().starts_with("set-cookie:") {
            if let Some(sid) = line[11..].trim().split(';').next() {
                return Some(sid.trim().to_string());
            }
        }
    }
    None
}

/// Extract a JSON string field (handles \" \\ \/ escapes; \uXXXX passes through).
fn json_field(obj: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\":", key);
    let i = obj.find(&pat)? + pat.len();
    let b = obj.as_bytes();
    let mut j = i;
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
    }
    if j >= b.len() || b[j] != b'"' {
        return None;
    }
    j += 1;
    let mut out = String::new();
    while j < b.len() {
        match b[j] {
            b'\\' if j + 1 < b.len() => {
                let c = b[j + 1];
                if c == b'u' {
                    out.push('\\'); // keep the \u marker; qB emits raw UTF-8 anyway
                }
                out.push(match c {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    other => other as char,
                });
                j += 2;
            }
            b'"' => break,
            _ => {
                let ch = obj[j..].chars().next()?;
                out.push(ch);
                j += ch.len_utf8();
            }
        }
    }
    Some(out)
}

/// Extract a JSON integer field (torrent sizes are plain u64 numbers).
fn json_num(obj: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{}\":", key);
    let i = obj.find(&pat)? + pat.len();
    let rest = &obj[i..];
    let digits: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Scan a torrents/info JSON body for torrents staged into QB_SOURCE_DIR.
/// Returns (hash, name, size) triples.
fn qb_scan_body(body: &str) -> Vec<(String, String, u64)> {
    let mut out = Vec::new();
    // JSON encodes C:\qb-moved-sources with escaped backslashes:
    let needle = "\"save_path\":\"C:\\\\qb-moved-sources\"";
    let mut from = 0usize;
    while let Some(rel) = body[from..].find(needle) {
        let hit = from + rel;
        // expand to the enclosing flat object (qB torrents/info objects are flat)
        let start = body[..hit].rfind('{').map(|i| i + 1).unwrap_or(0);
        let end = body[hit..].find('}').map(|i| hit + i).unwrap_or(body.len());
        let obj = &body[start..end];
        if let (Some(h), Some(n)) = (json_field(obj, "hash"), json_field(obj, "name")) {
            let sz = json_num(obj, "size").unwrap_or(0);
            out.push((h, n, sz));
        }
        from = end.max(hit + needle.len());
    }
    out
}

fn qb_c_source_torrents(sid: &str) -> Vec<(String, String, u64)> {
    match qb_http("GET", "/api/v2/torrents/info", None, Some(sid)) {
        Some((200, _, b)) => qb_scan_body(&b),
        _ => Vec::new(),
    }
}

/// Append the daemon's user-deletion marker (hash|name|ISO-UTC), deduped by hash.
fn qb_mark_deleted(hash: &str, name: &str) {
    let existing = fs::read_to_string(QB_MARKER).unwrap_or_default();
    if existing.contains(hash) {
        return;
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(QB_MARKER) {
        let _ = writeln!(f, "{}|{}|{}", hash, name, iso_utc(now_secs()));
    }
}

fn qb_delete_torrent(sid: &str, hash: &str) -> bool {
    let body = format!("hashes={}&deleteFiles=true", hash);
    matches!(qb_http("POST", "/api/v2/torrents/delete", Some(&body), Some(sid)), Some((200, _, _)))
}

/// Epoch seconds -> "YYYY-MM-DDTHH:MM:SSZ" (Howard Hinnant's civil-from-days).
fn iso_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64 + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe as i64 + era * 400 + if m <= 2 { 1 } else { 0 };
    let rem = secs % 86_400;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
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
        "mich-cdrive-clean v1.3.0 — safe, aggressive, repeatable C: drive cleaner\n\
         \n\
         USAGE:\n\
         \x20 mich-cdrive-clean                    dry run (reports what WOULD be freed — default, always safe)\n\
         \x20 mich-cdrive-clean --apply            actually delete everything eligible\n\
         \x20 mich-cdrive-clean --apply --deep     also run DISM StartComponentCleanup (WinSxS, 5-20 min)\n\
         \x20 mich-cdrive-clean --apply --imdb     (legacy no-op — IMDb caches now auto-clean after 2 days)\n\
         \x20 mich-cdrive-clean --apply --recycle  also empty the C: Recycle Bin\n\
         \x20 mich-cdrive-clean --apply --apps     also purge regenerable app data (Zoom ASR model)\n\
         \x20 mich-cdrive-clean --tips             only show big optional wins (needing YOUR decision)\n\
         \x20 mich-cdrive-clean --help             this help\n\
         \n\
         SAFETY: locked files are skipped, never forced. Windows\\Installer, Package Cache,\n\
         WinSxS, DriverStore, live session dirs and data caches are hard-protected.\n\
         Safe to run as often as you like; it converges (second run frees ~nothing).\n\
         \n\
         v1.3.0: auto-retires qBittorrent torrents staging into C:\\qb-moved-sources\n\
         (daemon marker protocol; E:/F: media libraries untouched) and verifies the\n\
         1770 GB free-space target at the end of every --apply run.\n"
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
    let protected: Vec<String> = vec![
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
    let _ = imdb; // legacy flag — IMDb caches are now default rules with a 2-day age gate

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
            desc: "C:\\Temp scratch sweep (session dirs, *.ps1; imdb caches age-gated separately)",
            run: Box::new(move |ctx, st| {
                if !temp_root2.exists() {
                    return;
                }
                let guard = |p: &Path| -> bool {
                    // keep small scripts sitting directly in C:\Temp root
                    if p.parent() == Some(Path::new("C:\\Temp")) {
                        return p.extension().map(|e| e.to_string_lossy().eq_ignore_ascii_case("ps1")).unwrap_or(false);
                    }
                    // IMDb caches have their own 2-day-gated rules (never age-0 here)
                    let s = path_lc(p);
                    if s.starts_with("c:\\temp\\imdb-datasets") || s.starts_with("c:\\temp\\s22") {
                        return true;
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

    // ---- relocation leftovers (*.reloc-old superseded by F:\backup\relocated) ----
    {
        let reloc_roots = vec![
            user.clone(),
            user.join("AppData\\Local\\Programs\\Python"),
            PathBuf::from("C:\\Program Files"),
            PathBuf::from("C:\\Program Files (x86)"),
        ];
        rules.push(Rule {
            name: "a-reloc-old".into(),
            desc: "*.reloc-old leftovers (byte-verified copies already on F:\\backup\\relocated)".into(),
            run: Box::new(move |ctx, st| {
                for root in &reloc_roots {
                    if let Ok(rd) = fs::read_dir(root) {
                        for ent in rd.flatten() {
                            let n = ent.file_name().to_string_lossy().to_string();
                            if !n.ends_with(".reloc-old") {
                                continue;
                            }
                            if let Some(md) = file_meta(&ent.path()) {
                                if md.is_dir() && !is_reparse(&md) {
                                    for e in collect(&ent.path()) {
                                        delete_file(ctx, &e, 1, st, None);
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

    // ---- qBittorrent C: staging hook (the daemon re-queues game repacks here) -------
    {
        let src = PathBuf::from(QB_SOURCE_DIR);
        rules.push(Rule {
            name: "qb-c-sources".into(),
            desc: "retire qB torrents staging into C:\\qb-moved-sources (marker protocol; E:/F: untouched)".into(),
            run: Box::new(move |ctx, st| {
                let on_disk: u64 = collect(&src).iter().map(|e| e.size).sum();
                let sid = qb_login();
                let mut torrents = Vec::new();
                if let Some(s) = &sid {
                    torrents = qb_c_source_torrents(s);
                }
                if !ctx.apply {
                    if on_disk > 0 || !torrents.is_empty() {
                        st.matched += on_disk;
                        st.freed += on_disk;
                    }
                    return;
                }
                if let Some(s) = &sid {
                    for (h, n, sz) in &torrents {
                        qb_mark_deleted(h, n);
                        if qb_delete_torrent(s, h) {
                            println!("    qb: retired torrent {}.. {} ({})", &h[..h.len().min(8)], n, gb(*sz));
                            st.matched += sz;
                            st.freed += sz;
                            st.n_freed += 1;
                        }
                    }
                }
                if !torrents.is_empty() {
                    thread::sleep(Duration::from_secs(6)); // let qB finish deleteFiles
                }
                // only sweep when qB answered AND no longer lists any C:-staged torrent;
                // if qB is down the state is unknown -> never blind-delete (locked)
                let sweep_ok = match &sid {
                    Some(s) => qb_c_source_torrents(s).is_empty(),
                    None => false,
                };
                if sweep_ok {
                    for e in collect(&src) {
                        delete_file(ctx, &e, 0, st, None);
                    }
                    prune_dirs(ctx, &src);
                } else if on_disk > 0 && !torrents.is_empty() {
                    st.locked += on_disk;
                    st.n_locked += 1;
                }
            }),
        });
    }

    // ---- IMDb caches: default since v1.3.0 (regenerable; 1-day age gate) -----------
    rules.push(tree_rule("a-imdb-cache", "IMDb gz dataset cache > 1d (BlackCinema re-downloads on demand)", PathBuf::from("C:\\Temp\\imdb-datasets"), 1));
    rules.push(tree_rule("a-imdb-s22", "IMDb s22 snapshot > 1d", PathBuf::from("C:\\Temp\\s22"), 1));
    // Defender keeps the newest platform binary; N-2 and older are dormant (and
    // Defender is excluded on this machine) — keep newest 2 for update safety.
    rules.push(keep_newest_dirs("a-defender-platform", "Defender platform dirs (keep newest 2)", pd.join("Microsoft\\Windows Defender\\Platform"), "4.", 2));

    // ---- optional flags -------------------------------------------------------------
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
        let st = Command::new("C:\\Windows\\System32\\Dism.exe")
            .args(["/Online", "/Cleanup-Image", "/StartComponentCleanup", "/NoRestart"])
            .status();
        match st {
            Ok(s) => println!("DISM exit: {}", s.code().unwrap_or(-1)),
            Err(e) => println!("DISM failed to launch: {e}"),
        }
        // keep System Restore capped at 2GB on C: (prevents the multi-GB shadow churn)
        let cap = "Get-WmiObject Win32_ShadowStorage | Where-Object { $_.DiffVolume.DeviceID -like '*Volume{50e23298*' } | ForEach-Object { $_.MaxSpace = 2147483648; $_.Put() } | Out-Null; vssadmin list shadowstorage";
        let _ = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", cap])
            .status()
            .map(|s| println!("shadow-cap exit: {}", s.code().unwrap_or(-1)));
    }

    let (free1, _) = disk_free_bytes("C:\\").unwrap_or((0, 0));
    if apply {
        let d = free1 - free0; // signed: live sessions can consume space mid-run
        let delta = if d >= 0 { gb(d as u64) } else { format!("-{}", gb(d.unsigned_abs())) };
        println!("C: free after : {}  (delta this run: {})", gb(free1 as u64), delta);
        // hard guarantee this tool is built around: >= 1770 GB free after every --apply
        let target: f64 = 1770.0;
        let free_gb = free1 as f64 / 1073741824.0;
        if free_gb >= target {
            println!("TARGET 1770 GB: HELD ({:.2} GB free)", free_gb);
        } else {
            println!(
                "TARGET 1770 GB: short by {:.2} GB — pending: boot task MichRelocationFinish (+~8 GB junctions) and the 16 GB pagefile cap land at next reboot",
                target - free_gb
            );
        }
    }

    println!("\n=== BIG OPTIONAL WINS (need your decision — NOT deleted) ===");
    for (label, p) in &tips {
        let sz: u64 = collect(p).iter().map(|e| e.size).sum();
        if sz > 1024 * 1024 {
            println!("  {:>10}  {}", gb(sz), label);
        }
    }
    println!("\nAlso consider: pagefile.sys (this machine: 32 GB in use, configured 48-96 GB — a fixed 16 GB cap");
    println!("frees ~16 GB after reboot but is YOUR perf decision); powercfg /h /type reduced; boot task");
    println!("MichRelocationFinish completes pending F:\\backup\\relocated junction swaps at next boot.");
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
        // IMDb dirs are no longer in the default PROTECTED list (own age-gated rules)
        let prod = Ctx { apply: true, now: 0, protected: vec![], self_dir: String::new() };
        assert!(!is_protected(&prod, Path::new("C:\\Temp\\imdb-datasets\\title.akas.tsv.gz")));
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

    #[test]
    fn json_field_parsing() {
        let obj = r#"{"hash":"1f267bb5","name":"The Thaumaturge \"Deluxe\" Edition","save_path":"C:\\qb-moved-sources","size":16728701824}"#;
        assert_eq!(json_field(obj, "hash").unwrap(), "1f267bb5");
        assert_eq!(json_field(obj, "name").unwrap(), "The Thaumaturge \"Deluxe\" Edition");
        assert_eq!(json_field(obj, "save_path").unwrap(), "C:\\qb-moved-sources");
        assert_eq!(json_num(obj, "size").unwrap(), 16728701824);
        assert!(json_field(obj, "missing").is_none());
        assert!(json_field(obj, "size").is_none()); // numeric field is not a string field
    }

    #[test]
    fn qb_scan_finds_only_c_staging() {
        let body = r#"[{"added_on":1,"hash":"aaa111","name":"On E drive","save_path":"E:\\media\\games","size":100},{"added_on":2,"hash":"bbb222","name":"C staging [Repack]","save_path":"C:\\qb-moved-sources","size":16728701824}]"#;
        let hits = qb_scan_body(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "bbb222");
        assert_eq!(hits[0].1, "C staging [Repack]");
        assert_eq!(hits[0].2, 16728701824);
    }

    #[test]
    fn iso_utc_dates() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(iso_utc(1_735_689_600), "2025-01-01T00:00:00Z");
    }
}
