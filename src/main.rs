// Memnir — shared Claude memory across machines + sessions over Tailscale.
// Single binary, pure std. Shells out to system rsync/ssh (no reinvention).
// Only memories tagged `metadata.scope: shared` sync; everything else is local.

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
const IDX_WARN: usize = 12_000; // always-on index tokens before it's flagged
const OVERSIZE: usize = 2_000; // a single memory above this is flagged for splitting
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
// peer = `user@host` of the other machine. Configured per-machine so no personal
// data is baked into the binary: env MEMNIR_PEER, else first line of ~/.claude/memnir.conf.
fn peer() -> String {
    if let Ok(p) = std::env::var("MEMNIR_PEER") {
        let p = p.trim().to_string();
        if !p.is_empty() { return p; }
    }
    fs::read_to_string(PathBuf::from(home()).join(".claude/memnir.conf")).ok()
        .and_then(|s| s.lines().map(|l| l.trim().to_string())
            .find(|l| !l.is_empty() && !l.starts_with('#')))
        .unwrap_or_default()
}
fn require_peer() -> Option<String> {
    let p = peer();
    if p.is_empty() {
        eprintln!("memnir: peer not configured — set env MEMNIR_PEER or write `user@host` to ~/.claude/memnir.conf");
        None
    } else {
        Some(p)
    }
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
    fm.lines().any(|l| {
        let t = l.trim();
        t.strip_prefix("scope:").map(|v| v.trim() == "shared").unwrap_or(false)
    })
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
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}
// Set/clear the `scope: shared` frontmatter line. Pure: returns the new file
// content, or None when there is no frontmatter block.
fn set_scope_in(content: &str, shared: bool) -> Option<String> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let (fm, tail) = (&rest[..end], &rest[end..]); // tail starts with "\n---"
    let mut lines: Vec<String> = fm.lines().map(str::to_string).collect();
    lines.retain(|l| l.trim_start().split(':').next().map(str::trim) != Some("scope"));
    if shared {
        // insert after the `type:` line, matching its indent; else append
        let anchor = lines.iter().enumerate().find(|(_, l)| l.trim_start().starts_with("type:"));
        let (idx, indent) = match anchor {
            Some((i, l)) => (i + 1, l[..l.len() - l.trim_start().len()].to_string()),
            None => (lines.len(), "  ".to_string()),
        };
        lines.insert(idx, format!("{}scope: shared", indent));
    }
    Some(format!("---\n{}{}", lines.join("\n"), tail))
}
// Pull the `<file>.md` out of a MEMORY.md index line `- [Title](<file>.md) — ...`.
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

// ---------- model / load ----------
struct Mem {
    file: String, // basename incl .md
    name: String,
    typ: String,
    desc: String,
    shared: bool,
    tok: usize,
    links: Vec<String>,
}
fn md_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(sm()).map(|rd| {
        rd.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "md")
                && p.file_name().is_some_and(|n| n != "MEMORY.md"))
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

// ---------- index ----------
fn regen_index() {
    let idx_path = sm().join("MEMORY.md");
    let mut keep: HashMap<String, String> = HashMap::new();
    if let Ok(cur) = fs::read_to_string(&idx_path) {
        for line in cur.lines() {
            if let Some(file) = index_file_of(line) {
                keep.insert(file.to_string(), line.to_string());
            }
        }
    }
    let mut out = String::from("# Memory Index\n\n");
    for m in load() {
        match keep.get(&m.file) {
            Some(l) => out.push_str(l),
            None => out.push_str(&format!("- [{}]({}) — {}", m.file.trim_end_matches(".md").replace('_', " "), m.file, m.desc)),
        }
        out.push('\n');
    }
    let _ = fs::write(idx_path, out);
}
fn set_scope(file: &Path, shared: bool) {
    let Ok(content) = fs::read_to_string(file) else { return };
    match set_scope_in(&content, shared) {
        Some(new) => { let _ = fs::write(file, new); }
        None => eprintln!("no frontmatter: {}", file.display()),
    }
}
// resolve → set scope → regen index → push (when shared). Single source of truth
// for share/local/toggle. Ok(true) = pushed to peer.
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
    let Some(p) = require_peer() else { return };
    rsync_files_from(&shared_files().join("\n"), &format!("{}/", sm().display()), &format!("{}:.claude/memnir/", p));
}
fn pull() {
    let Some(p) = require_peer() else { return };
    let out = Command::new("ssh").args(SSH_ARGS).arg(&p)
        .arg("cd ~/.claude/memnir && grep -lE '^[[:space:]]*scope:[[:space:]]*shared[[:space:]]*$' -- *.md 2>/dev/null")
        .output();
    let rlist = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    rsync_files_from(&rlist, &format!("{}:.claude/memnir/", p), &format!("{}/", sm().display()));
    regen_index();
}
fn peer_drift() -> Option<usize> {
    let p = peer();
    if p.is_empty() { return None; }
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
    for m in &mems {
        *types.entry(m.typ.clone()).or_insert(0) += 1;
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
        oversized: oversized.iter().map(|m| m.file.clone()).collect(),
        scope_flags,
        mems,
        edges,
    }
}

// ---------- commands ----------
fn cmd_status() {
    let a = analyze();
    println!("Memnir store: {}", sm().display());
    println!("memories: {}  (shared:{}  local:{})", a.n, a.shared, a.n - a.shared);
    let p = peer();
    println!("peer: {}", if p.is_empty() { "(unset — see ~/.claude/memnir.conf)".into() } else { p });
}
fn cmd_list() {
    let mems = load();
    println!("SHARED (sync ข้ามเครื่อง):");
    for m in mems.iter().filter(|m| m.shared) { println!("  {}", m.file); }
    println!("LOCAL (เครื่องนี้เท่านั้น):");
    for m in mems.iter().filter(|m| !m.shared) { println!("  {}", m.file); }
}
fn cmd_sync() {
    println!("host={}  peer={}", hostname(), peer());
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
            if pushed { println!("  pushed to peer"); }
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
    let drift = peer_drift();
    if check {
        let mut w = Vec::new();
        if a.idx_tok > IDX_WARN { w.push(format!("index {}k tok always-on", (a.idx_tok + 500) / 1000)); }
        if a.broken > 0 { w.push(format!("{} broken [[links]]", a.broken)); }
        if let Some(d) = drift { if d > 0 { w.push(format!("sync drift {} files", d)); } }
        if !w.is_empty() { println!("⚠ memnir: {}  → run `memnir doctor`", w.join("; ")); }
        return;
    }
    let dot = |v: usize, t: usize| if v > t { "🔴" } else if v * 10 > t * 6 { "🟠" } else { "🟢" };
    let p = peer();
    let pname = if p.is_empty() { "(unset)".to_string() } else { p };
    let peer_ok = if drift.is_some() { "✓" } else { "unreachable" };
    let drift_s = drift.map(|d| d.to_string()).unwrap_or_else(|| "?".into());
    let mut types: Vec<_> = a.types.iter().collect();
    types.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    let types = types.iter().map(|(k, c)| format!("{}:{}", k, c)).collect::<Vec<_>>().join(" ");
    println!("MEMNIR HEALTH ───────────────────────────────── {}", hostname());
    println!("inventory   {} memories   {}", a.n, types);
    println!("scope       shared:{}   local:{}", a.shared, a.n - a.shared);
    println!("tokens      index ~{:.1}k/session {}   pool ~{}k", a.idx_tok as f64 / 1000.0, dot(a.idx_tok, IDX_WARN), a.pool_tok / 1000);
    println!("sync        peer {} {}   drift: {} files", pname, peer_ok, drift_s);
    println!();
    println!("⚠ ISSUES & ACTIONS");
    if a.idx_tok > IDX_WARN {
        println!(" 🔴 index {}k always-on        → compact-index (Tier-0 split)", (a.idx_tok + 500) / 1000);
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
            jstr(&format!("{}  ~{}tok  {}", m.file, m.tok, if m.shared { "shared" } else { "local" }))
        )
    }).collect::<Vec<_>>().join(",");
    let edges = a.edges.iter().map(|(s, t)| format!("{{\"from\":{},\"to\":{}}}", jstr(s), jstr(t))).collect::<Vec<_>>().join(",");
    let types = a.types.iter().map(|(k, v)| format!("{}:{}", jstr(k), v)).collect::<Vec<_>>().join(",");
    let mut top: Vec<&Mem> = a.mems.iter().collect();
    top.sort_by_key(|m| std::cmp::Reverse(m.tok));
    let top_json = top.iter().take(TOP_N).map(|m| format!("[{},{}]", jstr(&m.file), m.tok)).collect::<Vec<_>>().join(",");
    let data = format!(
        "{{\"nodes\":[{}],\"edges\":[{}],\"types\":{{{}}},\"shared\":{},\"n\":{},\"idx_tok\":{},\"pool_tok\":{},\"broken\":{},\"isolated\":{},\"top\":[{}],\"serve\":{},\"token\":{}}}",
        nodes, edges, types, a.shared, a.n, a.idx_tok, a.pool_tok, a.broken, a.isolated.len(), top_json, serve, jstr(token)
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
  sync                push + pull shared memories with the peer, then rebuild the index
  push               send shared memories to the peer (one way)
  pull               fetch shared memories from the peer (one way)
  start              autolink current project + sync   (run by the SessionStart hook)

SCOPE                only `scope: shared` memories cross machines; default is local
  share <id>         mark a memory shared and push it to the peer
  local <id>         remove the tag — keep it on this machine only
  list               list shared vs local memories

PROJECT
  link               symlink the current project's memory dir into the pool

INSIGHT
  doctor [--check]   health report: tokens, broken links, oversized, suggested actions
                     (--check prints only when something needs attention; for hooks)
  dash               write a static dashboard.html (knowledge graph + token viz)
  serve [--port N]   interactive dashboard on 127.0.0.1 (click node = toggle, sync button)

INFO
  status             store path, memory counts (shared:local), peer
  help               show this help

CONFIG
  peer               read from ~/.claude/memnir.conf  (or env MEMNIR_PEER)  e.g. user@tailscale-host

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
        "doctor" => cmd_doctor(args.iter().any(|a| a == "--check")),
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
        assert_eq!(fm_field(fm, "type").as_deref(), Some("project")); // indented
        assert_eq!(fm_field(fm, "description").as_deref(), Some("hi there: ok")); // value may contain ':'
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
        // setting shared twice keeps exactly one scope line
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
}
