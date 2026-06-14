// Memnir — shared Claude memory across machines + sessions over Tailscale.
// Single binary, pure std. Shells out to system rsync/ssh (no reinvention).
// Only memories tagged `metadata.scope: shared` sync; everything else is local.
// Peers form a mesh: each machine lists every other in ~/.claude/memnir.conf.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SSH_E: &str = "ssh -o ConnectTimeout=8 -o BatchMode=yes -o StrictHostKeyChecking=accept-new";
const SSH_ARGS: [&str; 6] = ["-o", "ConnectTimeout=8", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=accept-new"];
const REMOTE_SHARED_LS: &str = "cd ~/.claude/memnir && grep -lE '^[[:space:]]*scope:[[:space:]]*shared[[:space:]]*$' -- *.md 2>/dev/null";
const IDX_WARN: usize = 12_000; // always-on index tokens before it's flagged
const OVERSIZE: usize = 2_000; // a single memory above this is flagged for splitting
const TIER0_TYPES: [&str; 2] = ["user", "feedback"]; // default always-on tier for compact-index
const TOP_N: usize = 12; // dashboard "top token footprint" rows
const LABEL_LEN: usize = 22; // graph node label truncation
const HTML: &str = include_str!("dashboard.template.html");

// ---------- paths / config ----------
fn home() -> String {
    std::env::var("HOME").expect("HOME not set")
}
fn sm() -> PathBuf {
    PathBuf::from(home()).join(".claude/memnir")
}
fn hostname() -> String {
    Command::new("hostname").arg("-s").output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}
// Human-readable label for the OS this binary runs on. WSL2 is still target_os
// "linux", so detect it at runtime via /proc/version (Claude Code runs Windows-side).
fn os_label() -> String {
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "macos" => {
            let chip = match arch { "aarch64" => "Apple Silicon", "x86_64" => "Intel", a => a };
            format!("macOS ({})", chip)
        }
        "linux" => {
            let wsl = fs::read_to_string("/proc/version")
                .map(|v| v.to_lowercase().contains("microsoft"))
                .unwrap_or(false);
            if wsl { "Linux (WSL2)".to_string() } else { format!("Linux ({})", arch) }
        }
        "windows" => format!("Windows ({})", arch),
        other => format!("{} ({})", other, arch),
    }
}
// All peers (the other machines). Mesh: every machine lists every other one.
// env MEMNIR_PEER (comma/space/newline separated) overrides; else every
// non-comment line of ~/.claude/memnir.conf.
fn peers() -> Vec<String> {
    if let Ok(p) = std::env::var("MEMNIR_PEER") {
        let v: Vec<String> = p.split([',', ' ', '\n', '\t']).map(str::trim)
            .filter(|s| !s.is_empty()).map(String::from).collect();
        if !v.is_empty() { return v; }
    }
    fs::read_to_string(PathBuf::from(home()).join(".claude/memnir.conf"))
        .map(|s| s.lines().map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect())
        .unwrap_or_default()
}
fn now_stamp() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

// ---------- pure parsing helpers (unit-tested) ----------
fn frontmatter(t: &str) -> &str {
    t.strip_prefix("---\n").and_then(|r| r.find("\n---").map(|e| &r[..e])).unwrap_or("")
}
fn fm_field(fm: &str, key: &str) -> Option<String> {
    fm.lines().find_map(|line| {
        let l = line.trim_start();
        l.strip_prefix(key)?.strip_prefix(':').map(|v| v.trim().to_string())
    })
}
fn has_scope_shared(fm: &str) -> bool {
    fm.lines().any(|l| l.trim().strip_prefix("scope:").map(|v| v.trim() == "shared").unwrap_or(false))
}
fn extract_links(t: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b = t.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'[' && b[i + 1] == b'[' {
            if let Some(end) = t[i + 2..].find("]]") {
                out.push(t[i + 2..i + 2 + end].trim().to_string());
                i = i + 2 + end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}
fn norm(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}
// Best canonical target for a broken `[[link]]`: a normalized substring match
// (≥5-char link, ≥4-char key, both directions) that hits exactly ONE memory.
// `targets` = (normalized-key, canonical-name). None = leave it alone (ambiguous
// or a deliberate forward-reference). Pure → unit-tested.
fn resolve_broken_link(link: &str, targets: &[(String, String)]) -> Option<String> {
    let nl = norm(link);
    if nl.len() < 5 { return None; }
    let mut hits: Vec<&String> = targets.iter()
        .filter(|(k, _)| k.len() >= 4 && (k.contains(&nl) || nl.contains(k.as_str())))
        .map(|(_, name)| name).collect();
    hits.sort();
    hits.dedup();
    if hits.len() == 1 && hits[0].as_str() != link { Some(hits[0].clone()) } else { None }
}
// Insert a frontmatter line after the last `type:`/`scope:` line (matching its
// indent), else append. Skips if `key:` already present. Pure → unit-tested.
fn insert_fm_line(content: &str, key: &str, value: &str) -> Option<String> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let (fm, tail) = (&rest[..end], &rest[end..]); // tail starts with "\n---"
    if fm.lines().any(|l| l.trim_start().starts_with(&format!("{}:", key))) {
        return None;
    }
    let mut lines: Vec<String> = fm.lines().map(str::to_string).collect();
    let anchor = lines.iter().enumerate().rev()
        .find(|(_, l)| { let t = l.trim_start(); t.starts_with("type:") || t.starts_with("scope:") });
    let (idx, indent) = match anchor {
        Some((i, l)) => (i + 1, l[..l.len() - l.trim_start().len()].to_string()),
        None => (lines.len(), "  ".to_string()),
    };
    lines.insert(idx, format!("{}{}: {}", indent, key, value));
    Some(format!("---\n{}{}", lines.join("\n"), tail))
}
// Set/clear the `scope: shared` line. Pure: returns new content, None if no frontmatter.
fn set_scope_in(content: &str, shared: bool) -> Option<String> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let (fm, tail) = (&rest[..end], &rest[end..]);
    let mut lines: Vec<String> = fm.lines().map(str::to_string).collect();
    let had = lines.len();
    lines.retain(|l| l.trim_start().split(':').next().map(str::trim) != Some("scope"));
    if !shared {
        return Some(format!("---\n{}{}", lines.join("\n"), tail));
    }
    let _ = had;
    let joined = format!("---\n{}{}", lines.join("\n"), tail);
    insert_fm_line(&joined, "scope", "shared").or(Some(joined))
}
fn set_origin_in(content: &str, host: &str) -> Option<String> {
    insert_fm_line(content, "origin", host)
}
// Pull `<file>.md` out of a MEMORY.md index line `- [Title](<file>.md) — ...`.
fn index_file_of(line: &str) -> Option<&str> {
    let s = line.find("](")? + 2;
    let e = line[s..].find(".md)")? + 3;
    Some(&line[s..s + e])
}
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(c) => { out.push(c as char); i += 3; }
                Err(_) => { out.push('%'); i += 1; }
            },
            b'+' => { out.push(' '); i += 1; }
            c => { out.push(c as char); i += 1; }
        }
    }
    out
}
fn jstr(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}
fn fmt_counts(m: &BTreeMap<String, usize>) -> String {
    let mut v: Vec<_> = m.iter().collect();
    v.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    v.iter().map(|(k, c)| format!("{}:{}", k, c)).collect::<Vec<_>>().join("  ")
}
// Score a memory against lowercased query tokens: filename/name=6, description=3,
// body=1 per token hit. Returns (score, which-fields-matched). Pure → unit-tested.
fn score_match(tokens: &[&str], file: &str, name: &str, desc: &str, body: &str) -> (i32, String) {
    let (fl, nm, dl, bl) = (file.to_lowercase(), name.to_lowercase(), desc.to_lowercase(), body.to_lowercase());
    let (mut score, mut n, mut d, mut b) = (0i32, false, false, false);
    for t in tokens {
        if fl.contains(t) || nm.contains(t) { score += 6; n = true; }
        if dl.contains(t) { score += 3; d = true; }
        if bl.contains(t) { score += 1; b = true; }
    }
    let mut f = Vec::new();
    if n { f.push("name"); }
    if d { f.push("desc"); }
    if b { f.push("body"); }
    (score, f.join("+"))
}

// ---------- model / load ----------
struct Mem {
    file: String, // basename incl .md
    name: String,
    typ: String,
    desc: String,
    origin: String,
    shared: bool,
    tok: usize,
    links: Vec<String>,
}
fn md_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(sm()).map(|rd| {
        rd.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "md")
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with("MEMORY")))
            .collect()
    }).unwrap_or_default();
    v.sort();
    v
}
fn load() -> Vec<Mem> {
    md_files().into_iter().filter_map(|p| {
        let t = fs::read_to_string(&p).ok()?;
        let file = p.file_name()?.to_string_lossy().to_string();
        let fm = frontmatter(&t);
        Some(Mem {
            name: fm_field(fm, "name").unwrap_or_else(|| file.trim_end_matches(".md").to_string()),
            typ: fm_field(fm, "type").unwrap_or_else(|| "?".into()),
            desc: fm_field(fm, "description").unwrap_or_default(),
            origin: fm_field(fm, "origin").unwrap_or_else(|| "?".into()),
            shared: has_scope_shared(fm),
            tok: t.chars().count() / 4,
            links: extract_links(&t),
            file,
        })
    }).collect()
}
fn shared_files() -> Vec<String> {
    load().into_iter().filter(|m| m.shared).map(|m| m.file).collect()
}
fn resolve(id: &str) -> PathBuf {
    let base = Path::new(id).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    sm().join(format!("{}.md", base.trim_end_matches(".md")))
}

// ---------- origin stamping ----------
// Stamp each memory created on THIS machine with `metadata.origin: <hostname>`.
// A per-machine baseline grandfathers everything present at first run as "?"
// (we genuinely don't know where pre-existing memories came from); only memories
// that appear later — i.e. newly written here — get a real origin, before they push.
fn stamp_origins() {
    let host = hostname();
    if host.is_empty() { return; }
    let base = sm().join(".origin_baseline");
    let known: HashSet<String> = fs::read_to_string(&base)
        .map(|s| s.lines().map(String::from).collect()).unwrap_or_default();
    let current = md_files();
    if known.is_empty() {
        let names: Vec<String> = current.iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect();
        let _ = fs::write(&base, names.join("\n"));
        return;
    }
    for p in current {
        let Some(fname) = p.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        if known.contains(&fname) { continue; }
        let Ok(content) = fs::read_to_string(&p) else { continue };
        if fm_field(frontmatter(&content), "origin").is_some() { continue; }
        if let Some(new) = set_origin_in(&content, &host) { let _ = fs::write(&p, new); }
    }
}

// ---------- index ----------
// Compact mode: a `.index_compact` marker means MEMORY.md (the always-on index)
// holds only Tier-0 memories; the full catalog moves to MEMORY.full.md, which is
// not auto-loaded but still on disk + fully searchable (`search` scans the pool,
// not the index). Marker body = comma/space list of Tier-0 types; empty = default.
fn compact_marker() -> PathBuf {
    sm().join(".index_compact")
}
fn compact_on() -> bool {
    compact_marker().exists()
}
fn tier0_types() -> Vec<String> {
    fs::read_to_string(compact_marker()).ok()
        .map(|s| s.split([',', ' ', '\n', '\t']).map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect::<Vec<_>>())
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| TIER0_TYPES.iter().map(|s| s.to_string()).collect())
}
// Partition (type, line) entries into (tier-0, rest) by type membership. Pure → unit-tested.
fn partition_tier0<'a>(items: &'a [(String, String)], tier0: &[String]) -> (Vec<&'a str>, Vec<&'a str>) {
    let (mut t0, mut rest) = (Vec::new(), Vec::new());
    for (typ, line) in items {
        if tier0.iter().any(|t| t == typ) { t0.push(line.as_str()); } else { rest.push(line.as_str()); }
    }
    (t0, rest)
}
fn regen_index() {
    let idx_path = sm().join("MEMORY.md");
    let full_path = sm().join("MEMORY.full.md");
    // Preserve any hand-curated lines (from either file) keyed by target file.
    let mut keep: HashMap<String, String> = HashMap::new();
    for p in [&idx_path, &full_path] {
        if let Ok(cur) = fs::read_to_string(p) {
            for line in cur.lines() {
                if let Some(file) = index_file_of(line) {
                    keep.entry(file.to_string()).or_insert_with(|| line.to_string());
                }
            }
        }
    }
    let mems = load();
    let line_of = |m: &Mem| -> String {
        keep.get(&m.file).cloned().unwrap_or_else(||
            format!("- [{}]({}) — {}", m.file.trim_end_matches(".md").replace('_', " "), m.file, m.desc))
    };
    if compact_on() {
        let tier0 = tier0_types();
        let items: Vec<(String, String)> = mems.iter().map(|m| (m.typ.clone(), line_of(m))).collect();
        let (t0, _) = partition_tier0(&items, &tier0);
        let head = format!(
            "# Memory Index (Tier-0)\n\n> Compacted: only {} memories are always-on. Full catalog in [MEMORY.full.md](MEMORY.full.md); every memory stays searchable via `memnir search`.\n\n",
            tier0.join("/"));
        let _ = fs::write(&idx_path, format!("{}{}\n", head, t0.join("\n")));
        let full: Vec<String> = items.iter().map(|(_, l)| l.clone()).collect();
        let _ = fs::write(&full_path, format!("# Memory Index — full catalog\n\n{}\n", full.join("\n")));
    } else {
        let body: Vec<String> = mems.iter().map(&line_of).collect();
        let _ = fs::write(&idx_path, format!("# Memory Index\n\n{}\n", body.join("\n")));
        let _ = fs::remove_file(&full_path); // no stray catalog when not compacted
    }
}
fn set_scope(file: &Path, shared: bool) {
    let Ok(content) = fs::read_to_string(file) else { return };
    match set_scope_in(&content, shared) {
        Some(new) => { let _ = fs::write(file, new); }
        None => eprintln!("no frontmatter: {}", file.display()),
    }
}
// resolve → set scope → regen index → push (when shared). Single source of
// truth for share/local/toggle. Ok(true) = pushed to peers.
fn apply_scope(id: &str, shared: bool) -> Result<bool, String> {
    let f = resolve(id);
    if !f.exists() {
        return Err(format!("not found: {}", id));
    }
    set_scope(&f, shared);
    regen_index();
    if shared {
        push();
    }
    Ok(shared)
}

// ---------- rsync / ssh ----------
fn rsync_files_from(list: &str, src: &str, dest: &str) {
    if list.trim().is_empty() { return; }
    let mut child = Command::new("rsync")
        .args(["-auz", "-e", SSH_E, "--files-from=-", src, dest])
        .stdin(Stdio::piped()).spawn().expect("spawn rsync");
    child.stdin.take().unwrap().write_all(list.as_bytes()).ok();
    child.wait().ok();
}
fn push() {
    stamp_origins();
    let ps = peers();
    if ps.is_empty() { eprintln!("memnir: no peers configured (~/.claude/memnir.conf)"); return; }
    let list = shared_files().join("\n");
    let src = format!("{}/", sm().display());
    for p in ps {
        rsync_files_from(&list, &src, &format!("{}:.claude/memnir/", p));
    }
}
fn pull() {
    let ps = peers();
    if ps.is_empty() { eprintln!("memnir: no peers configured (~/.claude/memnir.conf)"); return; }
    let dest = format!("{}/", sm().display());
    for p in &ps {
        let out = Command::new("ssh").args(SSH_ARGS).arg(p).arg(REMOTE_SHARED_LS).output();
        let rlist = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        rsync_files_from(&rlist, &format!("{}:.claude/memnir/", p), &dest);
    }
    regen_index();
}
fn peer_drift_to(p: &str) -> Option<usize> {
    let list = shared_files().join("\n");
    if list.trim().is_empty() { return Some(0); }
    let mut child = Command::new("rsync")
        .args(["-auzn", "--out-format=%n", "-e", SSH_E, "--files-from=-",
            &format!("{}/", sm().display()), &format!("{}:.claude/memnir/", p)])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().ok()?;
    child.stdin.take().unwrap().write_all(list.as_bytes()).ok();
    let out = child.wait_with_output().ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty() && !l.ends_with('/')).count())
}

// ---------- analysis ----------
struct Analysis {
    mems: Vec<Mem>,
    edges: Vec<(String, String)>,
    broken: usize,
    isolated: Vec<String>,
    idx_tok: usize,
    pool_tok: usize,
    n: usize,
    shared: usize,
    types: BTreeMap<String, usize>,
    origins: BTreeMap<String, usize>,
    oversized: Vec<String>,
    scope_flags: Vec<String>,
}
fn analyze() -> Analysis {
    let mems = load();
    let mut by_norm: HashMap<String, String> = HashMap::new();
    for m in &mems {
        by_norm.insert(norm(m.file.trim_end_matches(".md")), m.file.clone());
        by_norm.insert(norm(&m.name), m.file.clone());
    }
    let mut edges = Vec::new();
    let mut broken = 0usize;
    let mut linked: HashSet<String> = HashSet::new();
    for m in &mems {
        for l in &m.links {
            match by_norm.get(&norm(l)) {
                Some(tgt) => {
                    edges.push((m.file.clone(), tgt.clone()));
                    linked.insert(m.file.clone());
                    linked.insert(tgt.clone());
                }
                None => broken += 1,
            }
        }
    }
    let isolated = mems.iter().filter(|m| !linked.contains(&m.file)).map(|m| m.file.clone()).collect();
    let idx_tok = fs::read_to_string(sm().join("MEMORY.md")).map(|s| s.chars().count() / 4).unwrap_or(0);
    let mut types = BTreeMap::new();
    let mut origins = BTreeMap::new();
    for m in &mems {
        *types.entry(m.typ.clone()).or_insert(0) += 1;
        *origins.entry(m.origin.clone()).or_insert(0) += 1;
    }
    let mut oversized: Vec<&Mem> = mems.iter().filter(|m| m.tok > OVERSIZE).collect();
    oversized.sort_by_key(|m| std::cmp::Reverse(m.tok));
    let scope_flags = mems.iter().filter(|m| !m.shared
        && ["mimir", "mac_mini", "macmini", "machine", "checkout", "cadence", "zsh", "commit", "backup", "deploy"]
            .iter().any(|k| m.file.contains(k)))
        .map(|m| m.file.clone()).collect();
    Analysis {
        broken,
        isolated,
        idx_tok,
        pool_tok: mems.iter().map(|m| m.tok).sum(),
        n: mems.len(),
        shared: mems.iter().filter(|m| m.shared).count(),
        types,
        origins,
        oversized: oversized.iter().map(|m| m.file.clone()).collect(),
        scope_flags,
        mems,
        edges,
    }
}

// ---------- commands ----------
fn cmd_status() {
    let a = analyze();
    let ps = peers();
    println!("Memnir store: {}", sm().display());
    println!("memories: {}  (shared:{}  local:{})", a.n, a.shared, a.n - a.shared);
    println!("origins:  {}", fmt_counts(&a.origins));
    println!("peers:    {}", if ps.is_empty() { "(none — see ~/.claude/memnir.conf)".to_string() } else { ps.join(", ") });
}
fn cmd_list() {
    let mems = load();
    println!("SHARED (sync ข้ามเครื่อง):");
    for m in mems.iter().filter(|m| m.shared) { println!("  [{}]  {}", m.origin, m.file); }
    println!("LOCAL (เครื่องนี้เท่านั้น):");
    for m in mems.iter().filter(|m| !m.shared) { println!("  {}", m.file); }
}
fn cmd_sync() {
    let ps = peers();
    println!("host={}  os={}  peers={}", hostname(), os_label(), if ps.is_empty() { "(none)".to_string() } else { ps.join(",") });
    push();
    pull();
    let a = analyze();
    println!("✓ synced — shared:{}  local:{}  total:{}", a.shared, a.n - a.shared, a.n);
}
fn cmd_scope(id: &str, shared: bool) {
    match apply_scope(id, shared) {
        Ok(pushed) => {
            let name = resolve(id).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| id.to_string());
            println!("✓ {} → scope:{}", name, if shared { "shared" } else { "local" });
            if pushed { println!("  pushed to peers"); }
        }
        Err(e) => { eprintln!("{}", e); std::process::exit(1); }
    }
}
fn cmd_link(auto: bool) {
    let pwd = std::env::current_dir().unwrap();
    let enc = pwd.to_string_lossy().replace('/', "-");
    let d = PathBuf::from(home()).join(".claude/projects").join(&enc);
    let m = d.join("memory");
    if fs::symlink_metadata(&m).map(|md| md.file_type().is_symlink()).unwrap_or(false) {
        if !auto { println!("already linked: {}", m.display()); }
        return;
    }
    let _ = fs::create_dir_all(&d);
    if m.exists() {
        let _ = Command::new("rsync")
            .args(["-a", "--exclude", "MEMORY.md", &format!("{}/", m.display()), &format!("{}/", sm().display())])
            .status();
        let _ = fs::rename(&m, d.join(format!("memory.bak.{}", now_stamp())));
    }
    let _ = symlink(sm(), &m);
    if !auto { println!("✓ linked {} → {}", m.display(), sm().display()); }
}
fn cmd_doctor(check: bool) {
    let a = analyze();
    let health: Vec<(String, Option<usize>)> = peers().iter().map(|p| (p.clone(), peer_drift_to(p))).collect();
    let reachable = health.iter().filter(|(_, d)| d.is_some()).count();
    let drift_sum: usize = health.iter().filter_map(|(_, d)| *d).sum();
    if check {
        let mut w = Vec::new();
        if a.idx_tok > IDX_WARN { w.push(format!("index {}k tok always-on", (a.idx_tok + 500) / 1000)); }
        if a.broken > 0 { w.push(format!("{} broken [[links]]", a.broken)); }
        if drift_sum > 0 { w.push(format!("sync drift {} files", drift_sum)); }
        if !w.is_empty() { println!("⚠ memnir: {}  → run `memnir doctor`", w.join("; ")); }
        return;
    }
    let dot = |v: usize, t: usize| if v > t { "🔴" } else if v * 10 > t * 6 { "🟠" } else { "🟢" };
    println!("MEMNIR HEALTH ───────────────────────────────── {} · {}", hostname(), os_label());
    println!("inventory   {} memories   {}", a.n, fmt_counts(&a.types));
    println!("scope       shared:{}   local:{}", a.shared, a.n - a.shared);
    println!("origins     {}", fmt_counts(&a.origins));
    println!("tokens      index ~{:.1}k/session {}   pool ~{}k", a.idx_tok as f64 / 1000.0, dot(a.idx_tok, IDX_WARN), a.pool_tok / 1000);
    println!("peers       {} (reachable {})   drift: {} files", health.len(), reachable, drift_sum);
    if health.len() > 1 || reachable < health.len() {
        for (p, d) in &health {
            println!("            {} {}", if d.is_some() { "✓" } else { "✗" }, p);
        }
    }
    println!();
    println!("⚠ ISSUES & ACTIONS");
    if a.idx_tok > IDX_WARN {
        let hint = if compact_on() { "widen local / prune Tier-0" } else { "Tier-0 split" };
        println!(" 🔴 index {}k always-on        → memnir compact-index   ({})", (a.idx_tok + 500) / 1000, hint);
    }
    if a.broken > 0 {
        println!(" 🟠 {} broken [[links]]          → memnir fix-links", a.broken);
    }
    if !a.oversized.is_empty() {
        let tops: Vec<_> = a.oversized.iter().take(3).map(|s| s.trim_end_matches(".md")).collect();
        println!(" 🟠 {} oversized (>2k tok)      → memnir split <id>   ({}…)", a.oversized.len(), tops.join(", "));
    }
    if !a.isolated.is_empty() {
        println!(" 🟡 {} isolated memories       → link them (graph: memnir dash)", a.isolated.len());
    }
    if !a.scope_flags.is_empty() {
        let ex: Vec<_> = a.scope_flags.iter().take(3).map(|s| s.trim_end_matches(".md")).collect();
        println!(" 🟡 {} local look cross-machine → memnir share <id>  ({}…)", a.scope_flags.len(), ex.join(", "));
    }
    println!();
    println!("→ visualize:  memnir dash");
}
fn idx_tok_now() -> usize {
    fs::read_to_string(sm().join("MEMORY.md")).map(|s| s.chars().count() / 4).unwrap_or(0)
}
// compact-index: shrink the always-on MEMORY.md to a Tier-0 subset (by type),
// spilling the full catalog to MEMORY.full.md. `on=false` (--off) restores it.
fn cmd_compact_index(on: bool, types: Vec<String>) {
    let before = idx_tok_now();
    if on {
        let body = if types.is_empty() { TIER0_TYPES.join(",") } else { types.join(",") };
        let _ = fs::write(compact_marker(), format!("{}\n", body));
    } else {
        let _ = fs::remove_file(compact_marker());
    }
    regen_index();
    let after = idx_tok_now();
    if on {
        let tier0 = tier0_types();
        let mems = load();
        let kept = mems.iter().filter(|m| tier0.contains(&m.typ)).count();
        println!("✓ compact-index ON — Tier-0 = {}", tier0.join("/"));
        println!("  MEMORY.md       ~{:.1}k → ~{:.1}k tok/session ({} of {} memories)",
            before as f64 / 1000.0, after as f64 / 1000.0, kept, mems.len());
        println!("  MEMORY.full.md  full catalog — on-demand, still searchable via `memnir search`");
        if after > IDX_WARN { println!("  ⚠ still above {}k — widen what's local or prune Tier-0 types", IDX_WARN / 1000); }
    } else {
        println!("✓ compact-index OFF — MEMORY.md restored to full catalog (~{:.1}k tok/session)", after as f64 / 1000.0);
    }
}
// fix-links: repair broken [[links]] that have a single unambiguous target by
// normalized match. Dry-run by default; --apply rewrites the source files.
fn cmd_fix_links(apply: bool) {
    let mems = load();
    let mut resolvable: HashSet<String> = HashSet::new(); // norm keys that already match
    let mut targets: Vec<(String, String)> = Vec::new(); // (norm key, canonical name)
    for m in &mems {
        for key in [norm(m.file.trim_end_matches(".md")), norm(&m.name)] {
            resolvable.insert(key.clone());
            targets.push((key, m.name.clone()));
        }
    }
    let mut fixable: Vec<(String, String, String)> = Vec::new(); // (file, old, new)
    let mut unresolved: Vec<(String, String)> = Vec::new();
    for m in &mems {
        for l in &m.links {
            if resolvable.contains(&norm(l)) { continue; }
            match resolve_broken_link(l, &targets) {
                Some(new) => fixable.push((m.file.clone(), l.clone(), new)),
                None => unresolved.push((m.file.clone(), l.clone())),
            }
        }
    }
    if fixable.is_empty() && unresolved.is_empty() {
        println!("✓ no broken [[links]] — nothing to fix");
        return;
    }
    if !fixable.is_empty() {
        println!("{} fixable [[link]]{}{}:", fixable.len(), if fixable.len() == 1 { "" } else { "s" },
            if apply { " (applying)" } else { " (dry-run — pass --apply)" });
        for (file, old, new) in &fixable {
            println!("  {}:  [[{}]] → [[{}]]", file.trim_end_matches(".md"), old, new);
        }
    }
    if apply {
        let mut changed = 0usize;
        let mut by_file: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for (file, old, new) in &fixable {
            by_file.entry(file.clone()).or_default().push((old.clone(), new.clone()));
        }
        for (file, repls) in &by_file {
            let p = sm().join(file);
            let Ok(mut content) = fs::read_to_string(&p) else { continue };
            for (old, new) in repls {
                let from = format!("[[{}]]", old);
                if content.contains(&from) {
                    content = content.replace(&from, &format!("[[{}]]", new));
                    changed += 1;
                }
            }
            let _ = fs::write(&p, content);
        }
        regen_index();
        println!("✓ rewrote {} link{}", changed, if changed == 1 { "" } else { "s" });
    }
    if !unresolved.is_empty() {
        println!("{} unresolved (no unique match — likely forward-references, left as-is):", unresolved.len());
        for (file, old) in unresolved.iter().take(20) {
            println!("  {}:  [[{}]]", file.trim_end_matches(".md"), old);
        }
        if unresolved.len() > 20 { println!("  … +{} more", unresolved.len() - 20); }
    }
}

// ---------- search ----------
fn cmd_search(query: &str, expand: bool) {
    let ql = query.to_lowercase();
    let tokens: Vec<&str> = ql.split_whitespace().collect();
    if tokens.is_empty() { eprintln!("usage: memnir search <query> [--expand]"); std::process::exit(1); }
    let mut hits: Vec<(String, i32, String)> = Vec::new();
    for p in md_files() {
        let Ok(c) = fs::read_to_string(&p) else { continue };
        let file = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let fm = frontmatter(&c);
        let name = fm_field(fm, "name").unwrap_or_default();
        let desc = fm_field(fm, "description").unwrap_or_default();
        let (score, fields) = score_match(&tokens, &file, &name, &desc, &c);
        if score > 0 { hits.push((file, score, fields)); }
    }
    hits.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    hits.truncate(15);
    if hits.is_empty() { println!("no matches for \"{}\"", query); return; }
    let max = hits[0].1.max(1);
    println!("🔍 \"{}\" — {} match(es)", query, hits.len());
    for (file, score, fields) in &hits {
        let bar = "█".repeat(((score * 10 / max).max(1)) as usize);
        println!("  {:<46}{:<11}[{}]", file, bar, fields);
    }
    if expand {
        let a = analyze();
        let hitset: HashSet<String> = hits.iter().map(|h| h.0.clone()).collect();
        let mut nb: BTreeMap<String, String> = BTreeMap::new();
        for (f, t) in &a.edges {
            if hitset.contains(f) && !hitset.contains(t) { nb.entry(t.clone()).or_insert_with(|| f.clone()); }
            if hitset.contains(t) && !hitset.contains(f) { nb.entry(f.clone()).or_insert_with(|| t.clone()); }
        }
        if !nb.is_empty() {
            println!("  ── related via [[links]] ──");
            for (f, via) in nb { println!("  {:<46}↳ from {}", f, via); }
        }
    }
}
fn cmd_related(id: &str, depth: usize) {
    let a = analyze();
    let start = resolve(id).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if !a.mems.iter().any(|m| m.file == start) { eprintln!("not found: {}", id); std::process::exit(1); }
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for (f, t) in &a.edges {
        adj.entry(f.clone()).or_default().push(t.clone());
        adj.entry(t.clone()).or_default().push(f.clone());
    }
    let mut seen: HashMap<String, usize> = HashMap::from([(start.clone(), 0)]);
    let mut queue = std::collections::VecDeque::from([start.clone()]);
    while let Some(cur) = queue.pop_front() {
        let d = seen[&cur];
        if d >= depth { continue; }
        for n in adj.get(&cur).into_iter().flatten() {
            if !seen.contains_key(n) { seen.insert(n.clone(), d + 1); queue.push_back(n.clone()); }
        }
    }
    let mut out: Vec<(&String, &usize)> = seen.iter().filter(|(f, _)| **f != start).collect();
    out.sort_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(b.0)));
    println!("🔗 related to {} (≤{} hops):", start, depth);
    if out.is_empty() { println!("  (no [[links]] — isolated)"); }
    for (f, d) in out { println!("  {}{}", "  ".repeat(*d), f); }
}

// ---------- dashboard ----------
fn dash_html(serve: bool, token: &str) -> String {
    let a = analyze();
    let color = |t: &str| match t {
        "project" => "#14b8a6",
        "reference" => "#22c55e",
        "feedback" => "#f59e0b",
        _ => "#94a3b8",
    };
    let nodes = a.mems.iter().map(|m| {
        let label: String = m.file.trim_end_matches(".md").replace('_', " ").chars().take(LABEL_LEN).collect();
        format!(
            "{{\"id\":{},\"label\":{},\"group\":{},\"value\":{},\"shape\":\"dot\",\"color\":{{\"background\":{},\"border\":{}}},\"title\":{}}}",
            jstr(&m.file), jstr(&label), jstr(&m.typ), m.tok, jstr(color(&m.typ)),
            jstr(if m.shared { "#0f766e" } else { "#e11d48" }),
            jstr(&format!("{}  ~{}tok  {} · from {}", m.file, m.tok, if m.shared { "shared" } else { "local" }, m.origin))
        )
    }).collect::<Vec<_>>().join(",");
    let edges = a.edges.iter().map(|(s, t)| format!("{{\"from\":{},\"to\":{}}}", jstr(s), jstr(t))).collect::<Vec<_>>().join(",");
    let types = a.types.iter().map(|(k, v)| format!("{}:{}", jstr(k), v)).collect::<Vec<_>>().join(",");
    let origins = a.origins.iter().map(|(k, v)| format!("{}:{}", jstr(k), v)).collect::<Vec<_>>().join(",");
    let mut top: Vec<&Mem> = a.mems.iter().collect();
    top.sort_by_key(|m| std::cmp::Reverse(m.tok));
    let top_json = top.iter().take(TOP_N).map(|m| format!("[{},{}]", jstr(&m.file), m.tok)).collect::<Vec<_>>().join(",");
    let data = format!(
        "{{\"nodes\":[{}],\"edges\":[{}],\"types\":{{{}}},\"origins\":{{{}}},\"shared\":{},\"n\":{},\"idx_tok\":{},\"pool_tok\":{},\"broken\":{},\"isolated\":{},\"top\":[{}],\"serve\":{},\"token\":{},\"host\":{},\"os\":{}}}",
        nodes, edges, types, origins, a.shared, a.n, a.idx_tok, a.pool_tok, a.broken, a.isolated.len(), top_json, serve, jstr(token), jstr(&hostname()), jstr(&os_label())
    );
    HTML.replace("/*DATA*/", &data)
}
fn cmd_dash() {
    let out = sm().join("dashboard.html");
    let _ = fs::write(&out, dash_html(false, ""));
    println!("{}", out.display());
}

// ---------- serve (interactive dashboard) ----------
fn rand_token() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = fs::File::open("/dev/urandom") { let _ = f.read_exact(&mut b); }
    b.iter().map(|x| format!("{:02x}", x)).collect()
}
fn toggle_scope(id: &str) -> String {
    let f = resolve(id);
    if !f.exists() { return format!("not found: {}", id); }
    let cur = has_scope_shared(frontmatter(&fs::read_to_string(&f).unwrap_or_default()));
    match apply_scope(id, !cur) {
        Ok(pushed) => format!("{} → {}{}", id, if cur { "local" } else { "shared" }, if pushed { " (pushed)" } else { "" }),
        Err(e) => e,
    }
}
fn handle_conn(s: &mut TcpStream, token: &str) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    let n = s.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let line = req.lines().next().unwrap_or("");
    let target = line.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let qp = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{}=", k)));
    let (status, ctype, body) = if path == "/" {
        ("200 OK", "text/html; charset=utf-8", dash_html(true, token))
    } else if let Some(action) = path.strip_prefix("/api/") {
        if qp("t") != Some(token) {
            ("403 Forbidden", "application/json", "{\"ok\":false,\"msg\":\"bad token\"}".to_string())
        } else {
            let id = qp("id").map(urldecode).unwrap_or_default();
            let msg = match action {
                "sync" => { push(); pull(); "synced".to_string() }
                "toggle" => toggle_scope(&id),
                "share" => apply_scope(&id, true).map(|_| format!("{} → shared", id)).unwrap_or_else(|e| e),
                "local" => apply_scope(&id, false).map(|_| format!("{} → local", id)).unwrap_or_else(|e| e),
                _ => "unknown action".to_string(),
            };
            ("200 OK", "application/json", format!("{{\"ok\":true,\"msg\":{}}}", jstr(&msg)))
        }
    } else {
        ("404 Not Found", "text/plain", "not found".to_string())
    };
    let resp = format!("HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status, ctype, body.len(), body);
    let _ = s.write_all(resp.as_bytes());
}
fn cmd_serve(port: u16) {
    let token = rand_token();
    let addr = format!("127.0.0.1:{}", port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => { eprintln!("memnir serve: cannot bind {} — {}", addr, e); std::process::exit(1); }
    };
    let url = format!("http://{}/?t={}", addr, token);
    println!("Memnir interactive dashboard → {}", url);
    println!("(localhost only · Ctrl-C to stop)");
    let _ = Command::new("open").arg(&url).status();
    for mut s in listener.incoming().flatten() { handle_conn(&mut s, &token); }
}

fn cmd_help() {
    print!(r#"memnir — shared Claude memory across machines + sessions, over Tailscale

USAGE
  memnir <command> [args]

SYNC
  sync                push + pull shared memories with all peers, then rebuild the index
  push               send shared memories to every peer (one way)
  pull               fetch shared memories from every peer (one way)
  start              autolink current project + sync   (run by the SessionStart hook)

SCOPE                only `scope: shared` memories cross machines; default is local
  share <id>         mark a memory shared and push it to the peers
  local <id>         remove the tag — keep it on this machine only
  list               list shared vs local memories (shared show their origin machine)

SEARCH
  search <q> [--expand]    keyword search; --expand also pulls in [[link]]-related memories
  related <id> [--depth N] memories connected to <id> via [[links]] (default depth 2)

PROJECT
  link               symlink the current project's memory dir into the pool

INSIGHT
  doctor [--check]   health: tokens, broken links, oversized, origins, peers, actions
  dash               write a static dashboard.html (knowledge graph + token + origins)
  serve [--port N]   interactive dashboard on 127.0.0.1 (click node = toggle, sync button)

TOKENS
  compact-index [types] [--off]   cap the always-on index: keep only Tier-0 types in
                     MEMORY.md (default user,feedback), spill the rest to MEMORY.full.md
  fix-links [--apply]             repair broken [[links]] with one unambiguous target

INFO
  status             store path, memory counts, origins, peers
  help               show this help

CONFIG
  peers              ~/.claude/memnir.conf — one `user@tailscale-host` per line (mesh),
                     or env MEMNIR_PEER (comma/space separated)

EXAMPLES
  memnir share project_firestore_envs
  memnir doctor
  memnir serve
"#);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("sync");
    match cmd {
        "start" => { cmd_link(true); cmd_sync(); }
        "sync" => cmd_sync(),
        "push" => push(),
        "pull" => pull(),
        "share" => cmd_scope(args.get(2).expect("usage: memnir share <id>"), true),
        "local" => cmd_scope(args.get(2).expect("usage: memnir local <id>"), false),
        "link" => cmd_link(false),
        "autolink" => cmd_link(true),
        "status" => cmd_status(),
        "list" => cmd_list(),
        "search" => {
            let expand = args.iter().any(|a| a == "--expand");
            let q = args.iter().skip(2).filter(|s| !s.starts_with("--")).cloned().collect::<Vec<_>>().join(" ");
            cmd_search(&q, expand);
        }
        "related" => {
            let id = args.get(2).filter(|s| !s.starts_with("--"))
                .unwrap_or_else(|| { eprintln!("usage: memnir related <id> [--depth N]"); std::process::exit(1); });
            let depth = args.iter().position(|a| a == "--depth").and_then(|i| args.get(i + 1))
                .and_then(|p| p.parse().ok()).unwrap_or(2);
            cmd_related(id, depth);
        }
        "doctor" => cmd_doctor(args.iter().any(|a| a == "--check")),
        "compact-index" => {
            let off = args.iter().any(|a| a == "--off");
            let types: Vec<String> = args.iter().skip(2).filter(|s| !s.starts_with("--"))
                .flat_map(|s| s.split(',')).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            cmd_compact_index(!off, types);
        }
        "fix-links" => cmd_fix_links(args.iter().any(|a| a == "--apply")),
        "dash" => cmd_dash(),
        "serve" => {
            let port = args.iter().position(|a| a == "--port").and_then(|i| args.get(i + 1))
                .and_then(|p| p.parse().ok()).unwrap_or(7177);
            cmd_serve(port);
        }
        "help" | "-h" | "--help" => cmd_help(),
        other => { eprintln!("memnir: unknown command '{}'\n", other); cmd_help(); std::process::exit(1); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_extracts_block() {
        assert_eq!(frontmatter("---\nname: x\ntype: project\n---\nbody"), "name: x\ntype: project");
        assert_eq!(frontmatter("no frontmatter here"), "");
        assert_eq!(frontmatter("---\nunterminated"), "");
    }

    #[test]
    fn fm_field_reads_values_and_indent() {
        let fm = "name: foo\n  type: project\ndescription: hi there: ok";
        assert_eq!(fm_field(fm, "name").as_deref(), Some("foo"));
        assert_eq!(fm_field(fm, "type").as_deref(), Some("project"));
        assert_eq!(fm_field(fm, "description").as_deref(), Some("hi there: ok"));
        assert_eq!(fm_field(fm, "missing"), None);
    }

    #[test]
    fn scope_shared_detection() {
        assert!(has_scope_shared("type: project\n  scope: shared"));
        assert!(has_scope_shared("scope:shared"));
        assert!(!has_scope_shared("scope: local"));
        assert!(!has_scope_shared("type: project"));
    }

    #[test]
    fn extract_links_finds_and_trims_wikilinks() {
        assert_eq!(extract_links("see [[a]] and [[ b-c ]] end"), vec!["a", "b-c"]);
        assert_eq!(extract_links("none here"), Vec::<String>::new());
        assert_eq!(extract_links("[[unterminated"), Vec::<String>::new());
    }

    #[test]
    fn norm_strips_punct_and_lowercases() {
        assert_eq!(norm("Project_Firestore-Envs"), "projectfirestoreenvs");
        assert_eq!(norm("a-b_c"), norm("A B C"));
    }

    #[test]
    fn set_scope_adds_after_type_idempotently() {
        let src = "---\nname: x\nmetadata:\n  type: project\n---\nbody\n";
        let out = set_scope_in(src, true).unwrap();
        assert!(out.contains("  scope: shared"), "{out}");
        assert!(out.contains("body"));
        let twice = set_scope_in(&out, true).unwrap();
        assert_eq!(twice.matches("scope: shared").count(), 1);
    }

    #[test]
    fn set_scope_local_removes_tag() {
        let src = "---\ntype: project\n  scope: shared\n---\nx";
        let out = set_scope_in(src, false).unwrap();
        assert!(!out.contains("scope:"));
        assert!(out.contains("type: project"));
    }

    #[test]
    fn set_scope_none_without_frontmatter() {
        assert!(set_scope_in("plain text", true).is_none());
    }

    #[test]
    fn set_origin_adds_once() {
        let src = "---\nname: x\nmetadata:\n  type: project\n  scope: shared\n---\nbody";
        let out = set_origin_in(src, "MacBook").unwrap();
        assert!(out.contains("  origin: MacBook"), "{out}");
        // ordering: origin goes after scope
        assert!(out.find("scope: shared").unwrap() < out.find("origin: MacBook").unwrap());
        // already stamped → None
        assert!(set_origin_in(&out, "Other").is_none());
    }

    #[test]
    fn index_file_parsing() {
        assert_eq!(index_file_of("- [Title](foo_bar.md) — desc"), Some("foo_bar.md"));
        assert_eq!(index_file_of("- [a](x.md)"), Some("x.md"));
        assert_eq!(index_file_of("no link here"), None);
    }

    #[test]
    fn urldecode_handles_percent_and_plus() {
        assert_eq!(urldecode("a%20b+c"), "a b c");
        assert_eq!(urldecode("plain"), "plain");
        assert_eq!(urldecode("%2F"), "/");
        assert_eq!(urldecode("trailing%"), "trailing%");
    }

    #[test]
    fn jstr_escapes_specials() {
        assert_eq!(jstr("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(jstr("plain"), "\"plain\"");
    }

    #[test]
    fn score_match_weights_and_fields() {
        // token in name (6) + desc (3) + body (1)
        let (s, f) = score_match(&["primekg"], "primekg_graph_agent.md", "PrimeKG agent", "uses primekg", "body about primekg");
        assert_eq!(s, 10);
        assert_eq!(f, "name+desc+body");
        // no match
        assert_eq!(score_match(&["zzz"], "a.md", "a", "b", "c"), (0, String::new()));
        // body-only
        let (s2, f2) = score_match(&["tenant"], "a.md", "a", "b", "the tenant id");
        assert_eq!((s2, f2.as_str()), (1, "body"));
    }

    #[test]
    fn fmt_counts_sorts_desc() {
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), 2);
        m.insert("b".to_string(), 5);
        assert_eq!(fmt_counts(&m), "b:5  a:2");
    }

    #[test]
    fn partition_tier0_splits_by_type() {
        let items = vec![
            ("user".to_string(), "U".to_string()),
            ("project".to_string(), "P".to_string()),
            ("feedback".to_string(), "F".to_string()),
            ("reference".to_string(), "R".to_string()),
        ];
        let tier0 = vec!["user".to_string(), "feedback".to_string()];
        let (t0, rest) = partition_tier0(&items, &tier0);
        assert_eq!(t0, vec!["U", "F"]);
        assert_eq!(rest, vec!["P", "R"]);
        // empty tier0 → everything spills to rest
        let (none, all) = partition_tier0(&items, &[]);
        assert!(none.is_empty());
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn resolve_broken_link_unique_match_only() {
        let targets: Vec<(String, String)> = [("primekg-graph-agent", "primekg-graph-agent"), ("iris-fhir-r5-plan", "iris-fhir-r5-plan")]
            .iter().map(|(f, n)| (norm(f), n.to_string())).collect();
        // unique substring hit → resolves to canonical name
        assert_eq!(resolve_broken_link("primekg graph", &targets).as_deref(), Some("primekg-graph-agent"));
        // too short (<5 normalized chars) → left alone
        assert_eq!(resolve_broken_link("iris", &targets), None);
        // no match → forward-reference, left alone
        assert_eq!(resolve_broken_link("totally-new-note", &targets), None);
    }

    #[test]
    fn resolve_broken_link_ambiguous_is_skipped() {
        let targets: Vec<(String, String)> = [("eir-cds-cardio", "eir-cds-cardio"), ("eir-cds-router", "eir-cds-router")]
            .iter().map(|(f, n)| (norm(f), n.to_string())).collect();
        // "eircds" is a substring of both → ambiguous → None
        assert_eq!(resolve_broken_link("eir-cds", &targets), None);
    }

    #[test]
    fn tier0_types_parses_marker_body() {
        // exercises the same split logic the marker file uses
        let parsed: Vec<String> = "user, feedback project\n".split([',', ' ', '\n', '\t'])
            .map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect();
        assert_eq!(parsed, vec!["user", "feedback", "project"]);
    }
}
