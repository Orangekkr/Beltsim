// Netlist parser, two phases.
//
// Phase 1 (expand): reads files, resolves `use` imports, applies scoped
// aliases and defaults, collects `def` module bodies (which may nest), and
// flattens every `inst` into primitive statements with slash-prefixed
// names. Pins are recorded as aliases: (instance, pin) -> (node, port).
// Expansion is pure inlining: an instantiated module is tick-identical to
// writing its contents by hand.
//
// Scoping model (lexical): each file has its own scope of defaults and
// aliases; `use`d files never leak scope into the importer. A def captures
// the scope active at its DEFINITION site; statements inside its body see
// that captured scope plus any default/alias lines above them in the body.
// Nested defs are visible inside their enclosing def (and below), invisible
// outside.
//
// Phase 2 (build): the single-pass builder over the flat statement stream.
// Declaration order of the EXPANDED stream is entity id order.
//
// See SYNTAX.md for the language reference.

use crate::sim::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Clone)]
struct Stmt {
    file: String,
    line: usize,
    ctx: String, // instance path, empty at top level
    toks: Vec<String>,
}

impl Stmt {
    fn err(&self, msg: String) -> String {
        if self.ctx.is_empty() {
            format!("{}:{}: {}", self.file, self.line, msg)
        } else {
            format!("{}:{} (in {}): {}", self.file, self.line, self.ctx, msg)
        }
    }
}

#[derive(Clone)]
struct RawLine {
    line: usize,
    toks: Vec<String>,
}

#[derive(Clone, Default)]
struct Scope {
    /// statement kind ("edge", "source", ...) -> ordered key=value defaults.
    defaults: HashMap<String, Vec<(String, String)>>,
    /// token -> replacement tokens (spliced in place, single pass).
    aliases: HashMap<String, Vec<String>>,
}

impl Scope {
    fn set_default(&mut self, kind: &str, k: &str, v: &str) {
        let e = self.defaults.entry(kind.to_string()).or_default();
        if let Some(slot) = e.iter_mut().find(|(ek, _)| ek == k) {
            slot.1 = v.to_string();
        } else {
            e.push((k.to_string(), v.to_string()));
        }
    }

    fn apply_aliases(&self, toks: &[String]) -> Vec<String> {
        let mut out = Vec::with_capacity(toks.len());
        for t in toks {
            match self.aliases.get(t) {
                Some(rep) => out.extend(rep.iter().cloned()),
                None => out.push(t.clone()),
            }
        }
        out
    }
}

#[derive(Clone)]
struct ModuleDef {
    file: String,
    body: Vec<RawLine>,
    /// Scope captured at the definition site.
    scope: Scope,
}

/// Lexical chain of module namespaces during expansion.
struct DefEnv<'a> {
    local: &'a HashMap<String, ModuleDef>,
    parent: Option<&'a DefEnv<'a>>,
}

impl<'a> DefEnv<'a> {
    fn resolve(&self, name: &str) -> Option<&ModuleDef> {
        if let Some(d) = self.local.get(name) {
            return Some(d);
        }
        self.parent.and_then(|p| p.resolve(name))
    }
}

const DEFAULTABLE: [&str; 5] = ["edge", "source", "button", "container", "gate"];
const EDGE_ARROWS: [&str; 2] = [">", "->"];

fn tokenize(raw: &str) -> Vec<String> {
    raw.split('#')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

fn check_fresh_name(name: &str, what: &str, file: &str, ln: usize) -> Result<(), String> {
    if name.contains('/') {
        return Err(format!(
            "{}:{}: {} name {} may not contain '/' (reserved for instances)",
            file, ln, name, what
        ));
    }
    Ok(())
}

/// Merge scope defaults into a node/edge statement's tokens: keys already
/// present win; for edges, mk and rate form one exclusive group.
fn merge_defaults(scope: &Scope, kind: &str, kv_start: usize, toks: &mut Vec<String>) {
    let Some(defs) = scope.defaults.get(kind) else {
        return;
    };
    let mut present: HashSet<String> = HashSet::new();
    let mut has_rate_group = false;
    for t in &toks[kv_start.min(toks.len())..] {
        if let Some((k, _)) = t.split_once('=') {
            present.insert(k.to_string());
            if k == "mk" || k == "rate" {
                has_rate_group = true;
            }
        }
    }
    for (k, v) in defs {
        if present.contains(k) {
            continue;
        }
        if kind == "edge" && (k == "mk" || k == "rate") && has_rate_group {
            continue;
        }
        toks.push(format!("{}={}", k, v));
    }
}

/// Read a def body from a line iterator, honoring nested def/end pairs.
fn collect_body<I>(lines: &mut I, file: &str, def_ln: usize, name: &str) -> Result<Vec<RawLine>, String>
where
    I: Iterator<Item = (usize, Vec<String>)>,
{
    let mut body = Vec::new();
    let mut depth = 0usize;
    for (ln, toks) in lines.by_ref() {
        if toks.is_empty() {
            continue;
        }
        match toks[0].as_str() {
            "def" => {
                depth += 1;
                body.push(RawLine { line: ln, toks });
            }
            "end" => {
                if depth == 0 {
                    return Ok(body);
                }
                depth -= 1;
                body.push(RawLine { line: ln, toks });
            }
            _ => body.push(RawLine { line: ln, toks }),
        }
    }
    Err(format!("{}:{}: def {} never closed with end", file, def_ln, name))
}

#[derive(Default)]
struct Expander {
    defs: HashMap<String, ModuleDef>,
    out: Vec<Stmt>,
    /// (instance path, pin name) -> (full node name, optional port)
    pins: HashMap<(String, String), (String, Option<String>)>,
    visited_files: HashSet<PathBuf>,
    node_names: HashSet<String>,
    inst_names: HashSet<String>,
}

impl Expander {
    fn expand_text(
        &mut self,
        text: &str,
        file_label: &str,
        base_dir: &Path,
        top_level: bool,
    ) -> Result<(), String> {
        let mut scope = Scope::default();
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, tokenize(l)));
        while let Some((ln, raw_toks)) = lines.next() {
            if raw_toks.is_empty() {
                continue;
            }
            let toks = if raw_toks[0] == "alias" {
                raw_toks
            } else {
                scope.apply_aliases(&raw_toks)
            };
            match toks[0].as_str() {
                "alias" => self.stmt_alias(&toks, &mut scope, file_label, ln)?,
                "default" => self.stmt_default(&toks, &mut scope, file_label, ln)?,
                "use" => {
                    if toks.len() != 2 {
                        return Err(format!("{}:{}: use <path>", file_label, ln));
                    }
                    let p = base_dir.join(&toks[1]);
                    let canon = p.canonicalize().map_err(|e| {
                        format!("{}:{}: cannot use {}: {}", file_label, ln, toks[1], e)
                    })?;
                    if !self.visited_files.insert(canon.clone()) {
                        continue; // idempotent import
                    }
                    let sub = std::fs::read_to_string(&canon).map_err(|e| {
                        format!("{}:{}: cannot read {}: {}", file_label, ln, toks[1], e)
                    })?;
                    let sub_dir = canon.parent().unwrap_or(Path::new(".")).to_path_buf();
                    self.expand_text(&sub, &toks[1], &sub_dir, false)?;
                }
                "def" => {
                    if toks.len() != 2 {
                        return Err(format!("{}:{}: def <name>", file_label, ln));
                    }
                    check_fresh_name(&toks[1], "module", file_label, ln)?;
                    if self.defs.contains_key(&toks[1]) {
                        return Err(format!(
                            "{}:{}: duplicate module {}",
                            file_label, ln, toks[1]
                        ));
                    }
                    let body = collect_body(&mut lines, file_label, ln, &toks[1])?;
                    self.defs.insert(
                        toks[1].clone(),
                        ModuleDef {
                            file: file_label.to_string(),
                            body,
                            scope: scope.clone(),
                        },
                    );
                }
                "end" => return Err(format!("{}:{}: end outside def", file_label, ln)),
                "pin" => {
                    return Err(format!("{}:{}: pin only allowed inside def", file_label, ln))
                }
                "inst" => {
                    if toks.len() != 3 {
                        return Err(format!("{}:{}: inst <name> <module>", file_label, ln));
                    }
                    check_fresh_name(&toks[1], "instance", file_label, ln)?;
                    self.declare_inst(&toks[1], file_label, ln)?;
                    let empty = HashMap::new();
                    let env = DefEnv { local: &empty, parent: None };
                    let mut stack = Vec::new();
                    self.expand_inst(&toks[1], &toks[2], file_label, ln, &env, &mut stack)?;
                }
                "item" | "at" => {
                    self.emit(file_label, ln, "", toks);
                }
                "node" | "edge" | "display" | "probe" => {
                    if !top_level {
                        return Err(format!(
                            "{}:{}: used files may only contain item, def, default, and alias statements (found {})",
                            file_label, ln, toks[0]
                        ));
                    }
                    let mut toks = toks;
                    if toks.len() >= 2 {
                        check_fresh_name(&toks[1], &toks[0].clone(), file_label, ln)?;
                    }
                    if toks[0] == "node" && toks.len() >= 3 {
                        self.declare_node(&toks[1].clone(), file_label, ln)?;
                        let kind = toks[2].clone();
                        merge_defaults(&scope, &kind, 3, &mut toks);
                    } else if toks[0] == "edge" {
                        merge_defaults(&scope, "edge", 5, &mut toks);
                    }
                    self.emit(file_label, ln, "", toks);
                }
                other => {
                    return Err(format!("{}:{}: unknown statement {}", file_label, ln, other))
                }
            }
        }
        Ok(())
    }

    fn stmt_alias(
        &mut self,
        toks: &[String],
        scope: &mut Scope,
        file: &str,
        ln: usize,
    ) -> Result<(), String> {
        if toks.len() < 3 {
            return Err(format!("{}:{}: alias <name> <token>...", file, ln));
        }
        check_fresh_name(&toks[1], "alias", file, ln)?;
        scope
            .aliases
            .insert(toks[1].clone(), toks[2..].to_vec());
        Ok(())
    }

    fn stmt_default(
        &mut self,
        toks: &[String],
        scope: &mut Scope,
        file: &str,
        ln: usize,
    ) -> Result<(), String> {
        if toks.len() < 3 {
            return Err(format!("{}:{}: default <kind> key=value...", file, ln));
        }
        let kind = toks[1].as_str();
        if !DEFAULTABLE.contains(&kind) {
            return Err(format!(
                "{}:{}: cannot default {} (allowed: edge, source, button, container, gate)",
                file, ln, kind
            ));
        }
        for t in &toks[2..] {
            let (k, v) = t
                .split_once('=')
                .ok_or(format!("{}:{}: default takes key=value pairs, got {}", file, ln, t))?;
            if kind == "edge" && matches!(k, "rule" | "mouth" | "lift") {
                return Err(format!(
                    "{}:{}: {} cannot be defaulted (semantic, not plumbing)",
                    file, ln, k
                ));
            }
            scope.set_default(kind, k, v);
        }
        Ok(())
    }

    fn emit(&mut self, file: &str, line: usize, ctx: &str, toks: Vec<String>) {
        self.out.push(Stmt {
            file: file.to_string(),
            line,
            ctx: ctx.to_string(),
            toks,
        });
    }

    fn declare_node(&mut self, name: &str, file: &str, ln: usize) -> Result<(), String> {
        if self.inst_names.contains(name) {
            return Err(format!(
                "{}:{}: node {} collides with an instance name",
                file, ln, name
            ));
        }
        self.node_names.insert(name.to_string());
        Ok(())
    }

    fn declare_inst(&mut self, name: &str, file: &str, ln: usize) -> Result<(), String> {
        if self.node_names.contains(name) || !self.inst_names.insert(name.to_string()) {
            return Err(format!(
                "{}:{}: instance {} collides with an existing node or instance",
                file, ln, name
            ));
        }
        Ok(())
    }

    fn expand_inst(
        &mut self,
        prefix: &str,
        mod_name: &str,
        at_file: &str,
        at_line: usize,
        env: &DefEnv,
        stack: &mut Vec<String>,
    ) -> Result<(), String> {
        if stack.iter().any(|m| m == mod_name) {
            return Err(format!(
                "{}:{}: recursive module instantiation: {} -> {}",
                at_file,
                at_line,
                stack.join(" -> "),
                mod_name
            ));
        }
        let def = env
            .resolve(mod_name)
            .or_else(|| self.defs.get(mod_name))
            .cloned()
            .ok_or(format!("{}:{}: unknown module {}", at_file, at_line, mod_name))?;
        stack.push(mod_name.to_string());

        // Body-local scope starts from the definition-site capture.
        let mut scope = def.scope.clone();
        // Nested modules defined in this body, visible here and below.
        let mut local_defs: HashMap<String, ModuleDef> = HashMap::new();

        let mut i = 0usize;
        while i < def.body.len() {
            let rl = &def.body[i];
            i += 1;
            let toks = if rl.toks[0] == "alias" {
                rl.toks.clone()
            } else {
                scope.apply_aliases(&rl.toks)
            };
            let t: Vec<&str> = toks.iter().map(|s| s.as_str()).collect();
            let pfx = |n: &str| format!("{}/{}", prefix, n);
            let pfx_endpoint = |ep: &str| match ep.split_once('.') {
                Some((n, p)) => format!("{}/{}.{}", prefix, n, p),
                None => format!("{}/{}", prefix, ep),
            };
            match t[0] {
                "alias" => {
                    self.stmt_alias(&toks, &mut scope, &def.file, rl.line)?;
                }
                "default" => {
                    self.stmt_default(&toks, &mut scope, &def.file, rl.line)?;
                }
                "def" => {
                    if t.len() != 2 {
                        return Err(format!("{}:{}: def <name>", def.file, rl.line));
                    }
                    check_fresh_name(t[1], "module", &def.file, rl.line)?;
                    // Collect the nested body from the raw stream, honoring
                    // deeper nesting.
                    let mut sub = Vec::new();
                    let mut depth = 0usize;
                    let mut closed = false;
                    while i < def.body.len() {
                        let brl = def.body[i].clone();
                        i += 1;
                        match brl.toks[0].as_str() {
                            "def" => {
                                depth += 1;
                                sub.push(brl);
                            }
                            "end" => {
                                if depth == 0 {
                                    closed = true;
                                    break;
                                }
                                depth -= 1;
                                sub.push(brl);
                            }
                            _ => sub.push(brl),
                        }
                    }
                    if !closed {
                        return Err(format!(
                            "{}:{}: def {} never closed with end",
                            def.file, rl.line, t[1]
                        ));
                    }
                    local_defs.insert(
                        t[1].to_string(),
                        ModuleDef {
                            file: def.file.clone(),
                            body: sub,
                            scope: scope.clone(),
                        },
                    );
                }
                "node" => {
                    if t.len() < 3 {
                        return Err(format!("{}:{}: node <name> <kind> ...", def.file, rl.line));
                    }
                    check_fresh_name(t[1], "node", &def.file, rl.line)?;
                    let full = pfx(t[1]);
                    self.declare_node(&full, &def.file, rl.line)?;
                    let kind = t[2].to_string();
                    let mut out_toks: Vec<String> =
                        vec!["node".into(), full, kind.clone()];
                    out_toks.extend(t[3..].iter().map(|s| s.to_string()));
                    merge_defaults(&scope, &kind, 3, &mut out_toks);
                    self.out.push(Stmt {
                        file: def.file.clone(),
                        line: rl.line,
                        ctx: prefix.to_string(),
                        toks: out_toks,
                    });
                }
                "edge" => {
                    if t.len() < 5 || !EDGE_ARROWS.contains(&t[3]) {
                        return Err(format!(
                            "{}:{}: edge <name> <from> > <to> ...",
                            def.file, rl.line
                        ));
                    }
                    check_fresh_name(t[1], "edge", &def.file, rl.line)?;
                    let mut out_toks: Vec<String> = vec![
                        "edge".into(),
                        pfx(t[1]),
                        pfx_endpoint(t[2]),
                        ">".into(),
                        pfx_endpoint(t[4]),
                    ];
                    out_toks.extend(t[5..].iter().map(|s| s.to_string()));
                    merge_defaults(&scope, "edge", 5, &mut out_toks);
                    self.out.push(Stmt {
                        file: def.file.clone(),
                        line: rl.line,
                        ctx: prefix.to_string(),
                        toks: out_toks,
                    });
                }
                "display" => {
                    if t.len() < 4 {
                        return Err(format!(
                            "{}:{}: display <name> <kind> ...",
                            def.file, rl.line
                        ));
                    }
                    check_fresh_name(t[1], "display", &def.file, rl.line)?;
                    let mut out_toks: Vec<String> =
                        vec!["display".into(), pfx(t[1]), t[2].to_string()];
                    match t[2] {
                        "lamp" | "counter" | "meter" => {
                            out_toks.push(pfx_endpoint(t[3]));
                        }
                        "register" => {
                            let bits: Vec<String> =
                                t[3].split(',').map(|b| pfx_endpoint(b)).collect();
                            out_toks.push(bits.join(","));
                        }
                        other => {
                            return Err(format!(
                                "{}:{}: unknown display kind {}",
                                def.file, rl.line, other
                            ))
                        }
                    }
                    out_toks.extend(t[4..].iter().map(|s| s.to_string()));
                    self.out.push(Stmt {
                        file: def.file.clone(),
                        line: rl.line,
                        ctx: prefix.to_string(),
                        toks: out_toks,
                    });
                }
                "probe" => {
                    if t.len() != 2 {
                        return Err(format!("{}:{}: probe <edge>", def.file, rl.line));
                    }
                    self.out.push(Stmt {
                        file: def.file.clone(),
                        line: rl.line,
                        ctx: prefix.to_string(),
                        toks: vec!["probe".into(), pfx_endpoint(t[1])],
                    });
                }
                "pin" => {
                    if t.len() != 3 {
                        return Err(format!(
                            "{}:{}: pin <name> <node>[.<port>]",
                            def.file, rl.line
                        ));
                    }
                    check_fresh_name(t[1], "pin", &def.file, rl.line)?;
                    let (n, p) = match t[2].split_once('.') {
                        Some((n, p)) => (n, Some(p.to_string())),
                        None => (t[2], None),
                    };
                    let key = (prefix.to_string(), t[1].to_string());
                    if self.pins.contains_key(&key) {
                        return Err(format!(
                            "{}:{}: duplicate pin {} in instance {}",
                            def.file, rl.line, t[1], prefix
                        ));
                    }
                    self.pins.insert(key, (pfx(n), p));
                }
                "inst" => {
                    if t.len() != 3 {
                        return Err(format!("{}:{}: inst <name> <module>", def.file, rl.line));
                    }
                    check_fresh_name(t[1], "instance", &def.file, rl.line)?;
                    let full = pfx(t[1]);
                    self.declare_inst(&full, &def.file, rl.line)?;
                    let env2 = DefEnv { local: &local_defs, parent: Some(env) };
                    self.expand_inst(&full, t[2], &def.file, rl.line, &env2, stack)?;
                }
                "end" => unreachable!("consumed by body collection"),
                other => {
                    return Err(format!(
                        "{}:{}: '{}' not allowed inside def (node, edge, display, probe, pin, inst, def, default, alias)",
                        def.file, rl.line, other
                    ))
                }
            }
        }
        stack.pop();
        Ok(())
    }
}

/// Parse a netlist from a string. `use` paths resolve against the current
/// working directory.
pub fn parse(text: &str) -> Result<World, String> {
    parse_with_base(text, "<input>", Path::new("."))
}

/// Parse a netlist file. `use` paths resolve against the file's directory.
pub fn parse_file(path: &str) -> Result<World, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path, e))?;
    let base = Path::new(path).parent().unwrap_or(Path::new(".")).to_path_buf();
    parse_with_base(&text, path, &base)
}

fn parse_with_base(text: &str, label: &str, base_dir: &Path) -> Result<World, String> {
    let mut ex = Expander::default();
    ex.expand_text(text, label, base_dir, true)?;
    build(ex.out, ex.pins)
}

struct PendingSource {
    node: usize,
    rate: Option<u32>,
}

/// Follow a pin-forwarding chain to a concrete (node name, port).
fn chase_pin(
    pins: &HashMap<(String, String), (String, Option<String>)>,
    name: &str,
    port: Option<&str>,
    node_exists: impl Fn(&str) -> bool,
) -> Result<(String, Option<String>), String> {
    let mut cur_name = name.to_string();
    let mut cur_port = port.map(|p| p.to_string());
    for _ in 0..64 {
        if node_exists(&cur_name) {
            return Ok((cur_name, cur_port));
        }
        let p = cur_port.clone().ok_or(format!("unknown node {}", cur_name))?;
        match pins.get(&(cur_name.clone(), p.clone())) {
            Some((next_name, next_port)) => {
                cur_name = next_name.clone();
                cur_port = next_port.clone();
            }
            None => return Err(format!("unknown node or pin {}.{}", cur_name, p)),
        }
    }
    Err(format!("pin chain too deep starting at {}", name))
}

fn build(
    stmts: Vec<Stmt>,
    pins: HashMap<(String, String), (String, Option<String>)>,
) -> Result<World, String> {
    let mut w = World::default();
    let mut pending_sources: Vec<PendingSource> = Vec::new();
    let mut pending_ats: Vec<Stmt> = Vec::new();
    let mut used_in: Vec<[bool; 3]> = Vec::new();
    let mut used_out: Vec<[bool; 3]> = Vec::new();
    let mut in_edge_at: Vec<[usize; 3]> = Vec::new();
    let mut out_edge_at: Vec<[usize; 3]> = Vec::new();
    let mut kinds: Vec<String> = Vec::new();

    let resolve_endpoint = |w: &World,
                            name: &str,
                            port: Option<&str>|
     -> Result<(usize, Option<String>), String> {
        let (n, p) = chase_pin(&pins, name, port, |x| w.node_id(x).is_some())?;
        let nid = w.node_id(&n).unwrap();
        Ok((nid, p))
    };

    // Resolve an edge reference for displays/probes: an edge name, or a
    // pin/port reference like inst.pin or node.port, which resolves to the
    // edge attached at that port.
    let resolve_edge_ref = |w: &World,
                            kinds: &[String],
                            in_at: &[[usize; 3]],
                            out_at: &[[usize; 3]],
                            spec: &str|
     -> Result<usize, String> {
        if let Some(eid) = w.edge_id(spec) {
            return Ok(eid);
        }
        let (name, port) = match spec.split_once('.') {
            Some((n, p)) => (n, Some(p)),
            None => (spec, None),
        };
        let (n, p) = chase_pin(&pins, name, port, |x| w.node_id(x).is_some())
            .map_err(|_| format!("unknown edge, node, or pin {}", spec))?;
        let nid = w.node_id(&n).unwrap();
        let kind = kinds[nid].as_str();
        let try_out = resolve_out_port(kind, p.as_deref(), &n)
            .ok()
            .map(|idx| out_at[nid][idx])
            .filter(|&e| e != usize::MAX);
        let try_in = resolve_in_port(kind, p.as_deref(), &n)
            .ok()
            .map(|idx| in_at[nid][idx])
            .filter(|&e| e != usize::MAX);
        try_out
            .or(try_in)
            .ok_or(format!("{} has no edge attached", spec))
    };

    for stmt in &stmts {
        let toks: Vec<&str> = stmt.toks.iter().map(|s| s.as_str()).collect();
        match toks[0] {
            "item" => {
                if toks.len() != 2 {
                    return Err(stmt.err("item <name>".into()));
                }
                if matches!(toks[1], "any" | "undefined" | "overflow") {
                    return Err(stmt.err(format!(
                        "item may not be named {} (reserved rule keyword)",
                        toks[1]
                    )));
                }
                if w.item_id(toks[1]).is_some() {
                    continue; // idempotent
                }
                w.item_names.push(toks[1].to_string());
                if w.item_names.len() > u16::MAX as usize - 1 {
                    return Err(stmt.err("too many item types".into()));
                }
            }
            "node" => {
                if toks.len() < 3 {
                    return Err(stmt.err("node <name> <kind> ...".into()));
                }
                let name = toks[1].to_string();
                if w.node_id(&name).is_some() {
                    return Err(stmt.err(format!("duplicate node {}", name)));
                }
                let kv = parse_kv(&toks[3..]).map_err(|e| stmt.err(e))?;
                let kind_s = toks[2];
                let kind = match kind_s {
                    "source" => {
                        let item_name =
                            kv_get(&kv, "item").ok_or(stmt.err("source needs item=".into()))?;
                        let item = w
                            .item_id(item_name)
                            .ok_or(stmt.err(format!("unknown item {}", item_name)))?;
                        let rate = match kv_get(&kv, "rate") {
                            Some(r) => Some(parse_rate(r).map_err(|e| stmt.err(e))?),
                            None => None,
                        };
                        let limit = match kv_get(&kv, "limit") {
                            Some(v) => {
                                Some(v.parse::<u64>().map_err(|_| stmt.err("bad limit".into()))?)
                            }
                            None => None,
                        };
                        let start_tick =
                            secs_to_ticks(kv_get(&kv, "start"), 0.0).map_err(|e| stmt.err(e))?;
                        let stop_tick = secs_to_ticks(kv_get(&kv, "stop"), f64::INFINITY)
                            .map_err(|e| stmt.err(e))?;
                        pending_sources.push(PendingSource { node: w.nodes.len(), rate });
                        NodeKind::Source {
                            item,
                            period: TICK_RATE,
                            start_tick,
                            stop_tick,
                            limit,
                            emitted: 0,
                            pending: false,
                        }
                    }
                    "button" => {
                        let item_name =
                            kv_get(&kv, "item").ok_or(stmt.err("button needs item=".into()))?;
                        let item = w
                            .item_id(item_name)
                            .ok_or(stmt.err(format!("unknown item {}", item_name)))?;
                        NodeKind::Button { item, queued: 0 }
                    }
                    "sink" => NodeKind::Sink,
                    "splitter" => NodeKind::Splitter { rr: 0, out_bufs: [EMPTY; 3] },
                    "smartsplitter" => NodeKind::SmartSplitter {
                        rules: [Vec::new(), Vec::new(), Vec::new()],
                        rr: 0,
                        out_bufs: [EMPTY; 3],
                    },
                    "merger" => NodeKind::Merger { rr: 0, buf: EMPTY },
                    "pmerger" => NodeKind::PMerger { buf: EMPTY },
                    "container" => {
                        let cap = match kv_get(&kv, "cap") {
                            Some(v) => {
                                v.parse::<usize>().map_err(|_| stmt.err("bad cap".into()))?
                            }
                            None => 1000,
                        };
                        let mut fifo = VecDeque::new();
                        if let Some(pf) = kv_get(&kv, "prefill") {
                            let (it, n) = parse_prefill(pf, &w, None).map_err(|e| stmt.err(e))?;
                            for _ in 0..n {
                                fifo.push_back(it);
                            }
                        }
                        NodeKind::Container { cap, fifo }
                    }
                    "gate" => {
                        let open_at = match kv_get(&kv, "open_at") {
                            Some(v) => Some(parse_secs(v).map_err(|e| stmt.err(e))?),
                            None => None,
                        };
                        let close_at = match kv_get(&kv, "close_at") {
                            Some(v) => Some(parse_secs(v).map_err(|e| stmt.err(e))?),
                            None => None,
                        };
                        let initial_open = match kv_get(&kv, "initial") {
                            Some("closed") => false,
                            Some("open") => true,
                            None => open_at.is_none(),
                            Some(x) => {
                                return Err(
                                    stmt.err(format!("initial must be open|closed, got {}", x))
                                )
                            }
                        };
                        let nid = w.nodes.len();
                        if let Some(t) = open_at {
                            w.stimuli.push((t, Stimulus::GateOpen { node: nid }));
                        }
                        if let Some(t) = close_at {
                            w.stimuli.push((t, Stimulus::GateClose { node: nid }));
                        }
                        NodeKind::Gate { open: initial_open, buf: EMPTY }
                    }
                    other => return Err(stmt.err(format!("unknown node kind {}", other))),
                };
                w.nodes.push(Node { name, kind, ins: Vec::new(), outs: Vec::new() });
                used_in.push([false; 3]);
                used_out.push([false; 3]);
                in_edge_at.push([usize::MAX; 3]);
                out_edge_at.push([usize::MAX; 3]);
                kinds.push(kind_s.to_string());
            }
            "edge" => {
                if toks.len() < 5 || !EDGE_ARROWS.contains(&toks[3]) {
                    return Err(stmt.err(
                        "edge <name> <from>[.port] > <to>[.port] k=v...".into(),
                    ));
                }
                let name = toks[1].to_string();
                if w.edge_id(&name).is_some() {
                    return Err(stmt.err(format!("duplicate edge {}", name)));
                }
                let (from_name, from_port) = split_ref(toks[2]);
                let (to_name, to_port) = split_ref(toks[4]);
                let (from_node, from_port) =
                    resolve_endpoint(&w, from_name, from_port).map_err(|e| stmt.err(e))?;
                let (to_node, to_port) =
                    resolve_endpoint(&w, to_name, to_port).map_err(|e| stmt.err(e))?;

                let mut is_lift = false;
                let mut kv_toks: Vec<&str> = Vec::new();
                for t in &toks[5..] {
                    if *t == "lift" {
                        is_lift = true;
                    } else {
                        kv_toks.push(t);
                    }
                }
                let kv = parse_kv(&kv_toks).map_err(|e| stmt.err(e))?;

                let rate = if let Some(mk) = kv_get(&kv, "mk") {
                    let m: u8 = mk.parse().map_err(|_| stmt.err("bad mk".into()))?;
                    mk_rate_per_min(m).ok_or(stmt.err("mk must be 1..6".into()))?
                } else if let Some(r) = kv_get(&kv, "rate") {
                    parse_rate(r).map_err(|e| stmt.err(e))?
                } else {
                    return Err(stmt.err("edge needs mk= or rate= (or a default)".into()));
                };
                let period = period_from_rate_per_min(rate).map_err(|e| stmt.err(e))?;
                let slots: usize = kv_get(&kv, "slots")
                    .ok_or(stmt.err("edge needs slots= (or a default)".into()))?
                    .parse()
                    .map_err(|_| stmt.err("bad slots".into()))?;
                if slots == 0 {
                    return Err(stmt.err("slots must be >= 1".into()));
                }
                let has_mouth = match kv_get(&kv, "mouth") {
                    Some("0") | Some("1") if !is_lift => {
                        return Err(stmt.err("mouth= only valid on lift edges".into()))
                    }
                    Some("0") => false,
                    Some("1") => true,
                    Some(x) => return Err(stmt.err(format!("mouth must be 0|1, got {}", x))),
                    None => is_lift,
                };
                let mut slot_vec = vec![EMPTY; slots];
                let mut count = 0;
                if let Some(pf) = kv_get(&kv, "prefill") {
                    let (it, n) =
                        parse_prefill(pf, &w, Some(slots)).map_err(|e| stmt.err(e))?;
                    if n > slots {
                        return Err(stmt.err(format!(
                            "prefill {} exceeds {} slots (mouth not prefillable)",
                            n, slots
                        )));
                    }
                    for s in slot_vec.iter_mut().take(n) {
                        *s = it;
                    }
                    count = n;
                }

                let eid = w.edges.len();
                let out_idx = resolve_out_port(
                    &kinds[from_node],
                    from_port.as_deref(),
                    &w.nodes[from_node].name,
                )
                .map_err(|e| stmt.err(e))?;
                let in_idx = resolve_in_port(
                    &kinds[to_node],
                    to_port.as_deref(),
                    &w.nodes[to_node].name,
                )
                .map_err(|e| stmt.err(e))?;
                if used_out[from_node][out_idx] {
                    return Err(stmt.err(format!(
                        "output port already used on {}",
                        w.nodes[from_node].name
                    )));
                }
                if used_in[to_node][in_idx] {
                    return Err(stmt.err(format!(
                        "input port already used on {}",
                        w.nodes[to_node].name
                    )));
                }
                used_out[from_node][out_idx] = true;
                used_in[to_node][in_idx] = true;
                out_edge_at[from_node][out_idx] = eid;
                in_edge_at[to_node][in_idx] = eid;

                attach(&mut w.nodes[from_node].outs, out_idx, eid);
                attach(&mut w.nodes[to_node].ins, in_idx, eid);

                if kinds[from_node] == "smartsplitter" {
                    let rule_s = kv_get(&kv, "rule")
                        .ok_or(stmt.err("edges leaving a smartsplitter need rule=".into()))?;
                    let rules = parse_rule_list(&w, rule_s).map_err(|e| stmt.err(e))?;
                    if let NodeKind::SmartSplitter { rules: r, .. } =
                        &mut w.nodes[from_node].kind
                    {
                        r[out_idx] = rules;
                    }
                } else if kv_get(&kv, "rule").is_some() {
                    return Err(stmt.err("rule= only valid leaving a smartsplitter".into()));
                }

                w.edges.push(Edge::new(
                    name, from_node, to_node, period, slot_vec, has_mouth, count,
                ));
            }
            "display" => {
                if toks.len() < 4 {
                    return Err(
                        stmt.err("display <name> lamp|register|counter|meter ...".into())
                    );
                }
                let dname = toks[1].to_string();
                match toks[2] {
                    "lamp" => {
                        let eid =
                            resolve_edge_ref(&w, &kinds, &in_edge_at, &out_edge_at, toks[3])
                                .map_err(|e| stmt.err(e))?;
                        w.displays.push(Display::Lamp { name: dname, edge: eid });
                    }
                    "meter" => {
                        let eid =
                            resolve_edge_ref(&w, &kinds, &in_edge_at, &out_edge_at, toks[3])
                                .map_err(|e| stmt.err(e))?;
                        w.displays.push(Display::Meter { name: dname, edge: eid });
                    }
                    "register" => {
                        let mut bits = Vec::new();
                        for en in toks[3].split(',') {
                            let eid =
                                resolve_edge_ref(&w, &kinds, &in_edge_at, &out_edge_at, en)
                                    .map_err(|e| stmt.err(e))?;
                            bits.push(eid);
                        }
                        let kv = parse_kv(&toks[4..]).map_err(|e| stmt.err(e))?;
                        let one =
                            resolve_display_item(&w, &kv, "one").map_err(|e| stmt.err(e))?;
                        let zero =
                            resolve_display_item(&w, &kv, "zero").map_err(|e| stmt.err(e))?;
                        w.displays.push(Display::Register { name: dname, bits, one, zero });
                    }
                    "counter" => {
                        let nid = w
                            .node_id(toks[3])
                            .ok_or(stmt.err(format!("unknown node {}", toks[3])))?;
                        let kv = parse_kv(&toks[4..]).map_err(|e| stmt.err(e))?;
                        let item = match kv_get(&kv, "item") {
                            Some(iname) => Some(
                                w.item_id(iname)
                                    .ok_or(stmt.err(format!("unknown item {}", iname)))?,
                            ),
                            None => None,
                        };
                        w.displays.push(Display::Counter { name: dname, node: nid, item });
                    }
                    other => return Err(stmt.err(format!("unknown display kind {}", other))),
                }
            }
            "at" => {
                if toks.len() < 3 {
                    return Err(stmt.err("at <sec> <action> ...".into()));
                }
                pending_ats.push(stmt.clone());
            }
            "probe" => {
                if toks.len() != 2 {
                    return Err(stmt.err("probe <edge>".into()));
                }
                let eid = resolve_edge_ref(&w, &kinds, &in_edge_at, &out_edge_at, toks[1])
                    .map_err(|e| stmt.err(e))?;
                w.probes.push(eid);
            }
            other => return Err(stmt.err(format!("unknown statement {}", other))),
        }
    }

    for n in &mut w.nodes {
        n.ins.retain(|&e| e != usize::MAX);
        n.outs.retain(|&e| e != usize::MAX);
    }

    for (nid, n) in w.nodes.iter().enumerate() {
        let kind = &kinds[nid];
        match kind.as_str() {
            "source" | "button" => {
                if n.outs.len() != 1 || !n.ins.is_empty() {
                    return Err(format!("{} {} needs exactly one output", kind, n.name));
                }
            }
            "sink" => {
                if n.ins.len() != 1 || !n.outs.is_empty() {
                    return Err(format!("sink {} needs exactly one input", n.name));
                }
            }
            "splitter" | "smartsplitter" => {
                if n.ins.len() != 1 || n.outs.is_empty() {
                    return Err(format!(
                        "{} {} needs one input and at least one output",
                        kind, n.name
                    ));
                }
                let used = &used_out[nid];
                let hi = used.iter().rposition(|&u| u).unwrap();
                if used[..=hi].iter().any(|&u| !u) {
                    return Err(format!(
                        "{} {} must use out ports contiguously from out0",
                        kind, n.name
                    ));
                }
            }
            "merger" | "pmerger" => {
                if n.outs.len() != 1 || n.ins.is_empty() {
                    return Err(format!(
                        "{} {} needs at least one input and one output",
                        kind, n.name
                    ));
                }
            }
            "container" | "gate" => {
                if n.ins.len() != 1 || n.outs.len() != 1 {
                    return Err(format!(
                        "{} {} needs exactly one input and one output",
                        kind, n.name
                    ));
                }
            }
            _ => {}
        }
    }

    for ps in pending_sources {
        let rate = match ps.rate {
            Some(r) => r,
            None => {
                let e = w.nodes[ps.node].outs[0];
                let p = w.edges[e].period;
                (TICK_RATE * 60 / p) as u32
            }
        };
        let period = period_from_rate_per_min(rate)?;
        if let NodeKind::Source { period: p, .. } = &mut w.nodes[ps.node].kind {
            *p = period;
        }
    }

    for stmt in pending_ats {
        let toks: Vec<&str> = stmt.toks.iter().map(|s| s.as_str()).collect();
        let t = parse_secs(toks[1]).map_err(|e| stmt.err(e))?;
        let stim = parse_action(&w, &toks[2..], stmt.line).map_err(|e| stmt.err(e))?;
        w.stimuli.push((t, stim));
    }

    w.finalize();
    Ok(w)
}

/// Parse a comma-separated rule list: item names, any, undefined, overflow.
pub fn parse_rule_list(w: &World, s: &str) -> Result<Vec<Rule>, String> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let r = match p {
            "any" => Rule::Any,
            "undefined" => Rule::Undefined,
            "overflow" => Rule::Overflow,
            item_name => Rule::Item(
                w.item_id(item_name).ok_or(format!("unknown item {}", item_name))?,
            ),
        };
        out.push(r);
    }
    if out.is_empty() {
        return Err("empty rule list".into());
    }
    Ok(out)
}

/// Resolve "NODE.outN" into (node id, out port index) for a smartsplitter.
pub fn resolve_ss_port(w: &World, spec: &str) -> Result<(usize, usize), String> {
    let (name, port) = spec
        .split_once('.')
        .ok_or(format!("expected NODE.outN, got {}", spec))?;
    let nid = w.node_id(name).ok_or(format!("unknown node {}", name))?;
    if !matches!(w.nodes[nid].kind, NodeKind::SmartSplitter { .. }) {
        return Err(format!("{} is not a smartsplitter", name));
    }
    let idx = match port {
        "out0" => 0,
        "out1" => 1,
        "out2" => 2,
        other => return Err(format!("bad port {}", other)),
    };
    if idx >= w.nodes[nid].outs.len() {
        return Err(format!("{}.{} has no edge attached", name, port));
    }
    Ok((nid, idx))
}

/// Build a full new priority ordering for a PMerger from tier=edge assigns.
pub fn build_pm_order(
    w: &World,
    node_name: &str,
    assigns: &[(String, String)],
) -> Result<(usize, Vec<usize>), String> {
    let nid = w.node_id(node_name).ok_or(format!("unknown node {}", node_name))?;
    if !matches!(w.nodes[nid].kind, NodeKind::PMerger { .. }) {
        return Err(format!("{} is not a pmerger", node_name));
    }
    let mut tiers: [Option<usize>; 3] = [None, None, None];
    for (tier, edge_name) in assigns {
        let ti = match tier.as_str() {
            "high" => 0,
            "med" => 1,
            "low" => 2,
            other => return Err(format!("bad tier {}", other)),
        };
        let eid = w.edge_id(edge_name).ok_or(format!("unknown edge {}", edge_name))?;
        if !w.nodes[nid].ins.contains(&eid) {
            return Err(format!("edge {} is not an input of {}", edge_name, node_name));
        }
        if tiers[ti].is_some() {
            return Err(format!("tier {} assigned twice", tier));
        }
        tiers[ti] = Some(eid);
    }
    let new_ins: Vec<usize> = tiers.iter().flatten().copied().collect();
    if new_ins.len() != w.nodes[nid].ins.len() {
        return Err(format!(
            "must assign every input of {} to exactly one tier ({} attached, {} assigned)",
            node_name,
            w.nodes[nid].ins.len(),
            new_ins.len()
        ));
    }
    let mut sorted = new_ins.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() != new_ins.len() {
        return Err("an edge was assigned to two tiers".into());
    }
    Ok((nid, new_ins))
}

/// Parse an action body (used by `at` statements and the play REPL).
pub fn parse_action(w: &World, toks: &[&str], ln: usize) -> Result<Stimulus, String> {
    match toks.first().copied() {
        Some("rule") => {
            if toks.len() != 3 {
                return Err(format!("line {}: rule <node>.<outN> <rulelist>", ln));
            }
            let (node, port) = resolve_ss_port(w, toks[1])?;
            let rules = parse_rule_list(w, toks[2])?;
            Ok(Stimulus::SetRules { node, port, rules })
        }
        Some("priority") => {
            if toks.len() < 3 {
                return Err(format!(
                    "line {}: priority <pmerger> high=<edge> [med=] [low=]",
                    ln
                ));
            }
            let mut assigns = Vec::new();
            for t in &toks[2..] {
                let (k, v) = t
                    .split_once('=')
                    .ok_or(format!("line {}: expected tier=edge, got {}", ln, t))?;
                assigns.push((k.to_string(), v.to_string()));
            }
            let (node, new_ins) = build_pm_order(w, toks[1], &assigns)?;
            Ok(Stimulus::SetPriority { node, new_ins })
        }
        Some("open") | Some("close") => {
            if toks.len() != 2 {
                return Err(format!("line {}: {} <gate>", ln, toks[0]));
            }
            let nid = w
                .node_id(toks[1])
                .ok_or(format!("line {}: unknown node {}", ln, toks[1]))?;
            if !matches!(w.nodes[nid].kind, NodeKind::Gate { .. }) {
                return Err(format!("line {}: {} is not a gate", ln, toks[1]));
            }
            if toks[0] == "open" {
                Ok(Stimulus::GateOpen { node: nid })
            } else {
                Ok(Stimulus::GateClose { node: nid })
            }
        }
        Some("press") => {
            if toks.len() < 2 || toks.len() > 3 {
                return Err(format!("line {}: press <button> [n]", ln));
            }
            let nid = w
                .node_id(toks[1])
                .ok_or(format!("line {}: unknown node {}", ln, toks[1]))?;
            if !matches!(w.nodes[nid].kind, NodeKind::Button { .. }) {
                return Err(format!("line {}: {} is not a button", ln, toks[1]));
            }
            let n: u64 = match toks.get(2) {
                Some(v) => v.parse().map_err(|_| format!("line {}: bad press count", ln))?,
                None => 1,
            };
            Ok(Stimulus::Press { node: nid, n })
        }
        _ => Err(format!(
            "line {}: unknown action (rule|priority|open|close|press)",
            ln
        )),
    }
}

fn resolve_display_item(w: &World, kv: &Kv, key: &str) -> Result<Item, String> {
    match kv_get(kv, key) {
        Some(name) => w.item_id(name).ok_or(format!("unknown item {}", name)),
        None => w.item_id(key).ok_or(format!(
            "register needs {}=<item> (no item named '{}' to default to)",
            key, key
        )),
    }
}

fn attach(v: &mut Vec<usize>, idx: usize, eid: usize) {
    while v.len() <= idx {
        v.push(usize::MAX);
    }
    v[idx] = eid;
}

fn split_ref(s: &str) -> (&str, Option<&str>) {
    match s.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (s, None),
    }
}

fn resolve_out_port(kind: &str, port: Option<&str>, node: &str) -> Result<usize, String> {
    match (kind, port) {
        ("splitter" | "smartsplitter", Some("out0")) => Ok(0),
        ("splitter" | "smartsplitter", Some("out1")) => Ok(1),
        ("splitter" | "smartsplitter", Some("out2")) => Ok(2),
        ("splitter" | "smartsplitter", _) => Err(format!(
            "{} needs explicit out0|out1|out2 on {}",
            kind, node
        )),
        ("merger" | "pmerger", Some("out") | None) => Ok(0),
        ("source" | "button" | "container" | "gate", Some("out") | None) => Ok(0),
        ("sink", _) => Err(format!("sink {} has no outputs", node)),
        (_, Some(p)) => Err(format!("bad output port .{} on {} ({})", p, node, kind)),
        (_, None) => Ok(0),
    }
}

fn resolve_in_port(kind: &str, port: Option<&str>, node: &str) -> Result<usize, String> {
    match (kind, port) {
        ("merger", Some("in0")) => Ok(0),
        ("merger", Some("in1")) => Ok(1),
        ("merger", Some("in2")) => Ok(2),
        ("merger", _) => Err(format!("merger needs explicit in0|in1|in2 on {}", node)),
        ("pmerger", Some("high")) => Ok(0),
        ("pmerger", Some("med")) => Ok(1),
        ("pmerger", Some("low")) => Ok(2),
        ("pmerger", _) => Err(format!("pmerger needs explicit high|med|low on {}", node)),
        ("splitter" | "smartsplitter", Some("in") | None) => Ok(0),
        ("sink" | "container" | "gate", Some("in") | None) => Ok(0),
        ("source" | "button", _) => Err(format!("{} {} has no inputs", kind, node)),
        (_, Some(p)) => Err(format!("bad input port .{} on {} ({})", p, node, kind)),
        (_, None) => Ok(0),
    }
}

type Kv<'a> = Vec<(&'a str, &'a str)>;

fn parse_kv<'a>(toks: &[&'a str]) -> Result<Kv<'a>, String> {
    let mut out = Vec::new();
    for t in toks {
        let (k, v) = t
            .split_once('=')
            .ok_or(format!("expected key=value, got {}", t))?;
        out.push((k, v));
    }
    Ok(out)
}

fn kv_get<'a>(kv: &Kv<'a>, key: &str) -> Option<&'a str> {
    kv.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn parse_rate(s: &str) -> Result<u32, String> {
    let core = s.strip_suffix("/min").unwrap_or(s);
    core.parse::<u32>().map_err(|_| format!("bad rate {}", s))
}

fn parse_secs(s: &str) -> Result<u64, String> {
    let f: f64 = s.parse().map_err(|_| format!("bad seconds value {}", s))?;
    Ok((f * TICK_RATE as f64).round() as u64)
}

fn secs_to_ticks(v: Option<&str>, default_s: f64) -> Result<u64, String> {
    match v {
        None => {
            if default_s.is_infinite() {
                Ok(u64::MAX)
            } else {
                Ok((default_s * TICK_RATE as f64).round() as u64)
            }
        }
        Some(s) => parse_secs(s),
    }
}

fn parse_prefill(
    s: &str,
    w: &World,
    full_means: Option<usize>,
) -> Result<(Item, usize), String> {
    let (item_name, n_s) = s.split_once(':').ok_or("prefill needs item:count".to_string())?;
    let it = w.item_id(item_name).ok_or(format!("unknown item {}", item_name))?;
    let n = if n_s == "full" {
        full_means.ok_or("prefill full only valid on edges".to_string())?
    } else {
        n_s.parse::<usize>().map_err(|_| "bad prefill count".to_string())?
    };
    Ok((it, n))
}
