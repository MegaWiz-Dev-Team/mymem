// Memnir — shared Claude memory across machines + sessions over Tailscale.
// Single binary, pure std. Shells out to system rsync/ssh (no reinvention).
// Only memories tagged `metadata.scope: shared` sync; everything else is local.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SSH_E: &str = "ssh -o ConnectTimeout=8 -o BatchMode=yes -o StrictHostKeyChecking=accept-new";
const SSH_ARGS: [&str; 6] = [
    "-o", "ConnectTimeout=8", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=accept-new",
];
const IDX_WARN: usize = 12000;

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
    let conf = PathBuf::from(home()).join(".claude/memnir.conf");
    fs::read_to_string(conf).ok()
        .and_then(|s| s.lines().map(|l| l.trim().to_string())
            .find(|l| !l.is_empty() && !l.starts_with('#')))
        .unwrap_or_default()
}
fn require_peer() -> Option<String> {
    let p = peer();
    if p.is_empty() {
        eprintln!("memnir: peer not configured — set env MEMNIR_PEER or write `user@host` to ~/.claude/memnir.conf");
        None
    } else { Some(p) }
}
fn now_stamp() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

// ---------- model ----------
struct Mem {
    file: String,        // basename incl .md
    name: String,
    typ: String,
    desc: String,
    shared: bool,
    tok: usize,
    links: Vec<String>,
}

fn frontmatter(t: &str) -> &str {
    if let Some(rest) = t.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") { return &rest[..end]; }
    }
    ""
}
fn fm_field(fm: &str, key: &str) -> Option<String> {
    for line in fm.lines() {
        let l = line.trim_start();
        if let Some(v) = l.strip_prefix(&format!("{}:", key)) {
            return Some(v.trim().to_string());
        }
    }
    None
}
fn has_scope_shared(fm: &str) -> bool {
    fm.lines().any(|l| {
        let t = l.trim();
        t == "scope: shared" || t.starts_with("scope:") && t["scope:".len()..].trim() == "shared"
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

fn md_files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(sm()).map(|rd| {
        rd.filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map_or(false, |x| x == "md")
                && p.file_name().map_or(false, |n| n != "MEMORY.md"))
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
        let name = fm_field(fm, "name").unwrap_or_else(|| file.trim_end_matches(".md").to_string());
        Some(Mem {
            name,
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

// ---------- index ----------
fn regen_index() {
    let idx_path = sm().join("MEMORY.md");
    let mut keep: HashMap<String, String> = HashMap::new();
    if let Ok(cur) = fs::read_to_string(&idx_path) {
        for line in cur.lines() {
            if let Some(s) = line.find("](") {
                if let Some(e) = line[s + 2..].find(".md)") {
                    let file = &line[s + 2..s + 2 + e + 3];
                    keep.insert(file.to_string(), line.to_string());
                }
            }
        }
    }
    let mut out = String::from("# Memory Index\n\n");
    for m in load() {
        if let Some(l) = keep.get(&m.file) {
            out.push_str(l);
        } else {
            out.push_str(&format!("- [{}]({}) — {}", m.file.trim_end_matches(".md").replace('_', " "), m.file, m.desc));
        }
        out.push('\n');
    }
    let _ = fs::write(idx_path, out);
}

fn set_scope(file: &Path, shared: bool) {
    let t = match fs::read_to_string(file) { Ok(t) => t, Err(_) => return };
    let body = match t.strip_prefix("---\n").and_then(|r| r.find("\n---").map(|e| (&r[..e], &r[e..]))) {
        Some((b, rest)) => (b.to_string(), rest.to_string()),
        None => { eprintln!("no frontmatter: {}", file.display()); return; }
    };
    let (fm, rest) = body;
    let mut lines: Vec<String> = fm.lines().map(|s| s.to_string()).collect();
    lines.retain(|l| l.trim_start().split(':').next().map(|k| k.trim()) != Some("scope"));
    if shared {
        // insert after `type:` line preserving its indent, else append
        let mut idx = None;
        let mut indent = String::from("  ");
        for (i, l) in lines.iter().enumerate() {
            if l.trim_start().starts_with("type:") {
                indent = l[..l.len() - l.trim_start().len()].to_string();
                idx = Some(i + 1);
            }
        }
        let entry = format!("{}scope: shared", indent);
        match idx { Some(i) => lines.insert(i, entry), None => lines.push(entry) }
    }
    let new = format!("---\n{}{}", lines.join("\n"), rest);
    let _ = fs::write(file, new);
}

// ---------- rsync/ssh ----------
fn rsync_files_from(list: &str, src: &str, dest: &str) {
    if list.trim().is_empty() { return; }
    let mut child = Command::new("rsync")
        .args(["-auz", "-e", SSH_E, "--files-from=-", src, dest])
        .stdin(Stdio::piped()).spawn().expect("spawn rsync");
    child.stdin.take().unwrap().write_all(list.as_bytes()).ok();
    child.wait().ok();
}
fn push() {
    let p = match require_peer() { Some(p) => p, None => return };
    let list = shared_files().join("\n");
    rsync_files_from(&list, &format!("{}/", sm().display()), &format!("{}:.claude/memnir/", p));
}
fn pull() {
    let p = match require_peer() { Some(p) => p, None => return };
    let out = Command::new("ssh").args(SSH_ARGS).arg(&p)
        .arg("cd ~/.claude/memnir && grep -lE '^[[:space:]]*scope:[[:space:]]*shared[[:space:]]*$' -- *.md 2>/dev/null")
        .output();
    let rlist = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    rsync_files_from(&rlist, &format!("{}:.claude/memnir/", p), &format!("{}/", sm().display()));
    regen_index();
}
fn peer_drift() -> Option<usize> {
    // rsync dry-run of shared files; count would-transfer lines
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
    auto_fix: usize,
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
            if let Some(tgt) = by_norm.get(&norm(l)) {
                edges.push((m.file.clone(), tgt.clone()));
                linked.insert(m.file.clone());
                linked.insert(tgt.clone());
            } else {
                broken += 1;
            }
        }
    }
    let isolated: Vec<String> = mems.iter().filter(|m| !linked.contains(&m.file)).map(|m| m.file.clone()).collect();
    let idx_tok = fs::read_to_string(sm().join("MEMORY.md")).map(|s| s.chars().count() / 4).unwrap_or(0);
    let mut types = BTreeMap::new();
    for m in &mems { *types.entry(m.typ.clone()).or_insert(0) += 1; }
    let mut oversized: Vec<&Mem> = mems.iter().filter(|m| m.tok > 2000).collect();
    oversized.sort_by_key(|m| std::cmp::Reverse(m.tok));
    let flags: Vec<String> = mems.iter().filter(|m| !m.shared && {
        let f = &m.file;
        ["mimir","mac_mini","macmini","machine","checkout","cadence","zsh","commit","backup","deploy"]
            .iter().any(|k| f.contains(k))
    }).map(|m| m.file.clone()).collect();
    Analysis {
        broken, auto_fix: broken, // all are -/_ or missing; fix-links tool will resolve the auto ones
        isolated, idx_tok,
        pool_tok: mems.iter().map(|m| m.tok).sum(),
        n: mems.len(), shared: mems.iter().filter(|m| m.shared).count(),
        types, oversized: oversized.iter().map(|m| m.file.clone()).collect(),
        scope_flags: flags, mems, edges,
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
    regen_index();
    let a = analyze();
    println!("✓ synced — shared:{}  local:{}  total:{}", a.shared, a.n - a.shared, a.n);
}
fn resolve(id: &str) -> PathBuf {
    let base = Path::new(id).file_name().unwrap().to_string_lossy().to_string();
    let base = base.trim_end_matches(".md");
    sm().join(format!("{}.md", base))
}
fn cmd_scope(id: &str, shared: bool) {
    let f = resolve(id);
    if !f.exists() { eprintln!("not found: {}", f.display()); std::process::exit(1); }
    set_scope(&f, shared);
    regen_index();
    println!("✓ {} → scope:{}", f.file_name().unwrap().to_string_lossy(), if shared {"shared"} else {"local"});
    if shared { push(); println!("  pushed to peer"); }
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
        let _ = Command::new("rsync").args(["-a", "--exclude", "MEMORY.md",
            &format!("{}/", m.display()), &format!("{}/", sm().display())]).status();
        let _ = fs::rename(&m, d.join(format!("memory.bak.{}", now_stamp())));
    }
    let _ = symlink(sm(), &m);
    if !auto { println!("✓ linked {} → {}", m.display(), sm().display()); }
}
fn cmd_doctor(check: bool) {
    let a = analyze();
    let drift = peer_drift();
    let drift_s = drift.map(|d| d.to_string()).unwrap_or_else(|| "?".into());
    if check {
        let mut w = Vec::new();
        if a.idx_tok > IDX_WARN { w.push(format!("index {}k tok always-on", (a.idx_tok + 500) / 1000)); }
        if a.broken > 0 { w.push(format!("{} broken [[links]]", a.broken)); }
        if let Some(d) = drift { if d > 0 { w.push(format!("sync drift {} files", d)); } }
        if !w.is_empty() { println!("⚠ memnir: {}  → run `memnir doctor`", w.join("; ")); }
        return;
    }
    let dot = |v: usize, t: usize| if v > t { "🔴" } else if v * 10 > t * 6 { "🟠" } else { "🟢" };
    let pname = peer();
    let pname = if pname.is_empty() { "(unset)".to_string() } else { pname };
    let peer_ok = if drift.is_some() { "✓" } else { "unreachable" };
    println!("MEMNIR HEALTH ───────────────────────────────── {}", hostname());
    let types: Vec<String> = { let mut v: Vec<_> = a.types.iter().collect(); v.sort_by_key(|(_, c)| std::cmp::Reverse(**c)); v.iter().map(|(k, c)| format!("{}:{}", k, c)).collect() };
    println!("inventory   {} memories   {}", a.n, types.join(" "));
    println!("scope       shared:{}   local:{}", a.shared, a.n - a.shared);
    println!("tokens      index ~{:.1}k/session {}   pool ~{}k", a.idx_tok as f64 / 1000.0, dot(a.idx_tok, IDX_WARN), a.pool_tok / 1000);
    println!("sync        peer {} {}   drift: {} files", pname, peer_ok, drift_s);
    println!();
    println!("⚠ ISSUES & ACTIONS");
    if a.idx_tok > IDX_WARN {
        println!(" 🔴 index {}k always-on        → compact-index (Tier-0 split)", (a.idx_tok + 500) / 1000);
    }
    if a.broken > 0 {
        println!(" 🟠 {} broken [[links]]          → memnir fix-links  (~{} auto -/_)", a.broken, a.auto_fix);
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

// minimal JSON string escape
fn jstr(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c { '"' => o.push_str("\\\""), '\\' => o.push_str("\\\\"), '\n' => o.push_str("\\n"), '\t' => o.push_str("\\t"), c => o.push(c) }
    }
    o.push('"'); o
}
fn dash_html(serve: bool, token: &str) -> String {
    let a = analyze();
    let color = |t: &str| match t { "project" => "#14b8a6", "reference" => "#22c55e", "feedback" => "#f59e0b", _ => "#94a3b8" };
    let mut nodes = String::from("[");
    for (i, m) in a.mems.iter().enumerate() {
        if i > 0 { nodes.push(','); }
        let label: String = m.file.trim_end_matches(".md").replace('_', " ").chars().take(22).collect();
        nodes.push_str(&format!(
            "{{\"id\":{},\"label\":{},\"group\":{},\"value\":{},\"shape\":\"dot\",\"color\":{{\"background\":{},\"border\":{}}},\"title\":{}}}",
            jstr(&m.file), jstr(&label), jstr(&m.typ), m.tok, jstr(color(&m.typ)),
            jstr(if m.shared { "#0f766e" } else { "#e11d48" }),
            jstr(&format!("{}  ~{}tok  {}", m.file, m.tok, if m.shared {"shared"} else {"local"}))
        ));
    }
    nodes.push(']');
    let mut edges = String::from("[");
    for (i, (s, t)) in a.edges.iter().enumerate() {
        if i > 0 { edges.push(','); }
        edges.push_str(&format!("{{\"from\":{},\"to\":{}}}", jstr(s), jstr(t)));
    }
    edges.push(']');
    let types: Vec<String> = a.types.iter().map(|(k, v)| format!("{}:{}", jstr(k), v)).collect();
    let mut top: Vec<&Mem> = a.mems.iter().collect();
    top.sort_by_key(|m| std::cmp::Reverse(m.tok));
    let top_json: Vec<String> = top.iter().take(12).map(|m| format!("[{},{}]", jstr(&m.file), m.tok)).collect();
    let data = format!(
        "{{\"nodes\":{},\"edges\":{},\"types\":{{{}}},\"shared\":{},\"n\":{},\"idx_tok\":{},\"pool_tok\":{},\"broken\":{},\"isolated\":{},\"top\":[{}],\"serve\":{},\"token\":{}}}",
        nodes, edges, types.join(","), a.shared, a.n, a.idx_tok, a.pool_tok, a.broken, a.isolated.len(), top_json.join(","), serve, jstr(token)
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
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(c) = u8::from_str_radix(&s[i + 1..i + 3], 16) { out.push(c as char); i += 3; continue; }
                out.push('%'); i += 1;
            }
            b'+' => { out.push(' '); i += 1; }
            c => { out.push(c as char); i += 1; }
        }
    }
    out
}
fn toggle_scope(id: &str) -> String {
    let f = resolve(id);
    if !f.exists() { return format!("not found: {}", id); }
    let t = fs::read_to_string(&f).unwrap_or_default();
    let shared = has_scope_shared(frontmatter(&t));
    set_scope(&f, !shared);
    regen_index();
    if !shared { push(); format!("{} → shared (pushed)", id) } else { format!("{} → local", id) }
}
fn handle_conn(s: &mut std::net::TcpStream, token: &str) {
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
                "share" => { let f = resolve(&id); if f.exists() { set_scope(&f, true); regen_index(); push(); format!("{} → shared", id) } else { format!("not found: {}", id) } }
                "local" => { let f = resolve(&id); if f.exists() { set_scope(&f, false); regen_index(); format!("{} → local", id) } else { format!("not found: {}", id) } }
                _ => "unknown action".to_string(),
            };
            ("200 OK", "application/json", format!("{{\"ok\":true,\"msg\":{}}}", jstr(&msg)))
        }
    } else {
        ("404 Not Found", "text/plain", "not found".to_string())
    };
    let resp = format!("HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status, ctype, body.as_bytes().len(), body);
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
    for stream in listener.incoming() {
        if let Ok(mut s) = stream { handle_conn(&mut s, &token); }
    }
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

const HTML: &str = r###"<!doctype html><html><head><meta charset=utf-8>
<title>Memnir Dashboard</title>
<link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 64 64'><path d='M32 5 q4.5 7.5 4.5 12 a4.5 4.5 0 1 1 -9 0 Q27.5 12.5 32 5 z' fill='%230f766e'/><circle cx='32' cy='37' r='24' fill='none' stroke='%2314b8a6' stroke-width='3.5'/><circle cx='32' cy='37' r='16' fill='none' stroke='%232dd4a7' stroke-width='2.5'/><circle cx='32' cy='37' r='8.5' fill='none' stroke='%235eead4' stroke-width='2.5'/><circle cx='32' cy='37' r='3' fill='%230f766e'/></svg>">
<script src="https://unpkg.com/vis-network/standalone/umd/vis-network.min.js"></script>
<style>
 body{margin:0;background:#f4faf7;color:#16322b;font:14px -apple-system,system-ui,sans-serif}
 header{padding:14px 20px;border-bottom:1px solid #d3ece1;display:flex;gap:16px;align-items:center;background:#ffffff}
 h1{font-size:18px;margin:0}
 .cards{display:flex;gap:12px;flex-wrap:wrap;padding:16px 20px}
 .card{background:#ffffff;border:1px solid #d3ece1;border-radius:12px;padding:14px 16px;min-width:140px;box-shadow:0 1px 3px rgba(20,184,166,.06)}
 .card .v{font-size:24px;font-weight:700;color:#0f766e} .card .l{color:#6b8c80;font-size:12px}
 .warn{color:#e11d48 !important}.ok{color:#10b981 !important}
 .bar{height:9px;border-radius:5px;background:#e6f4ee;overflow:hidden;margin-top:6px}
 .bar>span{display:block;height:100%}
 .wrap{display:flex;gap:16px;padding:0 20px 20px;flex-wrap:wrap}
 .panel{background:#ffffff;border:1px solid #d3ece1;border-radius:12px;padding:14px}
 #graph{flex:1;min-width:520px;height:560px}
 .side{width:300px} .row{display:flex;justify-content:space-between;margin:4px 0;font-size:13px}
 .legend span{display:inline-block;width:10px;height:10px;border-radius:50%;margin-right:5px}
 button{background:#14b8a6;color:#fff;border:0;border-radius:8px;padding:6px 12px;font:13px inherit;cursor:pointer}
 button:hover{background:#0f9e8e}
 #toast{margin-left:10px;color:#0f766e;font-size:13px}
 .hint{color:#6b8c80;font-size:12px;margin-left:6px}
 .cmdrow{display:flex;align-items:center;gap:8px;margin:5px 0;font-size:12px}
 .cmdrow code{background:#e6f4ee;color:#0f766e;padding:1px 6px;border-radius:5px;white-space:nowrap}
 .cmddesc{color:#3f5b52;flex:1}
 .tag{color:#6b8c80;font-size:11px;border:1px solid #d3ece1;border-radius:5px;padding:0 6px;white-space:nowrap}
 .cmdrow button{padding:3px 10px;font-size:12px}
</style></head><body>
<header><h1><svg width="24" height="24" viewBox="0 0 64 64" style="vertical-align:-5px;margin-right:9px"><path d="M32 5 q4.5 7.5 4.5 12 a4.5 4.5 0 1 1 -9 0 Q27.5 12.5 32 5 z" fill="#0f766e"/><circle cx="32" cy="37" r="24" fill="none" stroke="#14b8a6" stroke-width="3.5"/><circle cx="32" cy="37" r="16" fill="none" stroke="#2dd4a7" stroke-width="2.5" opacity=".9"/><circle cx="32" cy="37" r="8.5" fill="none" stroke="#5eead4" stroke-width="2.5"/><circle cx="32" cy="37" r="3" fill="#0f766e"/></svg>Memnir Dashboard</h1><span id=sub class=l style="color:#6b8c80"></span><span id=tools style="margin-left:auto"></span></header>
<div class=cards id=cards></div>
<div class=wrap>
 <div class="panel" id=graph></div>
 <div class="panel side">
   <b>Types</b><div id=types></div>
   <b>Top token footprint</b><div id=top></div>
   <div class=legend style="margin-top:10px">
     <span style="background:#14b8a6"></span>project
     <span style="background:#22c55e"></span>reference
     <span style="background:#f59e0b"></span>feedback &nbsp; <span style="background:#e11d48"></span>=local border
   </div>
   <b style="display:block;margin-top:16px">Commands</b><div id=cmds></div>
   <div class=hint style="display:block;margin:8px 0 0 0">node = คลิก node ใน graph · graph = แผนที่นี้ · header = ด้านบน · hook = อัตโนมัติ · cli = พิมพ์ใน terminal · here = หน้านี้</div>
 </div>
</div>
<script>
const D=/*DATA*/;
const $=s=>document.querySelector(s);
$('#sub').textContent=`${D.n} memories · shared ${D.shared}/${D.n} · ${D.edges.length} links`;
const idxK=(D.idx_tok/1000).toFixed(1), warn=D.idx_tok>12000;
$('#cards').innerHTML=`
 <div class=card><div class="v ${warn?'warn':'ok'}">${idxK}k</div><div class=l>index tok / session ${warn?'🔴':'🟢'}</div>
   <div class=bar><span style="width:${Math.min(100,D.idx_tok/200)}%;background:${warn?'#e11d48':'#10b981'}"></span></div></div>
 <div class=card><div class=v>${(D.pool_tok/1000)|0}k</div><div class=l>pool total tok</div></div>
 <div class=card><div class=v>${D.shared}<span style="color:#6b8c80">/${D.n}</span></div><div class=l>shared / total</div></div>
 <div class=card><div class="v ${D.broken?'warn':'ok'}">${D.broken}</div><div class=l>broken links</div></div>
 <div class=card><div class=v>${D.isolated}</div><div class=l>isolated nodes</div></div>`;
const mx=Math.max(...Object.values(D.types));
$('#types').innerHTML=Object.entries(D.types).sort((a,b)=>b[1]-a[1]).map(([k,v])=>
 `<div class=row><span>${k}</span><span>${v}</span></div><div class=bar><span style="width:${v/mx*100}%;background:#14b8a6"></span></div>`).join('');
const tmx=Math.max(...D.top.map(t=>t[1]));
$('#top').innerHTML=D.top.map(([f,t])=>
 `<div class=row><span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:200px">${f}</span><span>${t}</span></div>
  <div class=bar><span style="width:${t/tmx*100}%;background:#2dd4a7"></span></div>`).join('');
const CMDS=[['sync','push + pull shared','run'],['share <id>','mark shared + push','node'],['local <id>','keep on this machine','node'],['list','shared vs local','graph'],['doctor','health + actions','refresh'],['status','counts / peer','header'],['link','join current project','hook'],['start','autolink + sync','hook'],['dash','static html file','cli'],['serve','this page','here'],['help','all commands','cli']];
$('#cmds').innerHTML=CMDS.map(([c,d,a])=>{
 if(!D.serve&&(a==='run'||a==='refresh'))a='cli';
 const btn=a==='run'?'<button onclick="act(\'sync\')">run</button>':a==='refresh'?'<button onclick="location.reload()">run</button>':`<span class=tag>${a}</span>`;
 return `<div class=cmdrow><code>${c.replace(/</g,'&lt;').replace(/>/g,'&gt;')}</code><span class=cmddesc>${d}</span>${btn}</div>`;
}).join('');
const net=new vis.Network($('#graph'),{nodes:new vis.DataSet(D.nodes),edges:new vis.DataSet(D.edges)},{
 nodes:{scaling:{min:6,max:34},font:{color:'#16322b',size:11}},
 edges:{color:{color:'#bfe6d8',highlight:'#14b8a6'},smooth:false,width:0.5},
 physics:{barnesHut:{gravitationalConstant:-8000,springLength:120},stabilization:{iterations:180}},
 interaction:{hover:true,tooltipDelay:80}});
function act(cmd,id){return fetch('/api/'+cmd+'?t='+D.token+(id?'&id='+encodeURIComponent(id):''),{method:'POST'}).then(r=>r.json()).then(j=>{const t=$('#toast');if(t)t.textContent=j.msg||'';setTimeout(()=>location.reload(),500);}).catch(()=>{});}
if(D.serve){
 $('#tools').innerHTML='<button onclick="act(\'sync\')">⟳ Sync</button> <button onclick="location.reload()">Refresh</button><span class=hint>click a node = toggle shared/local</span><span id=toast></span>';
 net.on('click',p=>{if(p.nodes.length)act('toggle',p.nodes[0]);});
}
</script></body></html>"###;
