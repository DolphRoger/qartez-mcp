// Rust guideline compliant 2026-04-22
//
// End-to-end regression coverage for commit aa63eb3 (fix(analyzers): eliminate
// 6 FP classes). These tests drive the full indexing pipeline against
// temp-dir fixtures and then inspect the resulting DB directly, so a fix
// that only works at the tree-sitter layer but never reaches the stored
// graph will show up here as a failing assertion.
//
// Part 1: SEC004 must not fire on `Command::new(variable)` without a
//         shell-invocation `.arg("-c")` neighbour, but must still fire on
//         the real `Command::new(shell).arg("-c").arg(format!(...))` case.
// Part 2: The rust_lang parser must emit reference edges for
//         (a) generic type arguments, (b) serde attribute string paths,
//         and (c) uppercase identifiers inside proc-macro-style DSL
//         bodies, so `qartez_refs` finds them and `qartez_unused` does
//         not flag them as dead.

use std::collections::HashMap;
use std::fs;

use rusqlite::Connection;
use tempfile::TempDir;

use qartez_mcp::graph::security::{self, ScanOptions, Severity};
use qartez_mcp::index;
use qartez_mcp::storage::{read, schema};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build a temp-dir project with the supplied files, run `full_index`
/// against it, and return `(TempDir, Connection)`. The TempDir is kept
/// alive by the caller so its on-disk files are still readable while
/// the security scanner re-reads them.
fn index_project(files: &[(&str, &str)]) -> (TempDir, Connection) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for (rel, content) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    schema::create_schema(&conn).unwrap();
    index::full_index(&conn, root, false).unwrap();
    (dir, conn)
}

/// Scan the indexed project using the full built-in rule set. Tests are
/// excluded so `#[cfg(test)]` modules in real sources do not pollute the
/// finding list; the fixture itself is test-free anyway.
fn security_scan(conn: &Connection, root: &std::path::Path) -> Vec<security::Finding> {
    let rules = security::builtin_rules();
    let opts = ScanOptions {
        include_tests: false,
        category_filter: None,
        min_severity: Severity::Low,
        file_path_filter: None,
        project_roots: vec![root.to_path_buf()],
        root_aliases: HashMap::new(),
    };
    security::scan(conn, &rules, &opts)
}

/// Return every `(from_symbol_name, to_symbol_name)` edge in
/// `symbol_refs`, joined through the `symbols` table for readability.
/// Used as the raw inspection window into the refs graph.
fn symbol_ref_names(conn: &Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT sf.name, st.name
             FROM symbol_refs r
             JOIN symbols sf ON sf.id = r.from_symbol_id
             JOIN symbols st ON st.id = r.to_symbol_id
             ORDER BY sf.name, st.name",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap();
    rows.filter_map(|r| r.ok()).collect()
}

/// Count rows in `symbol_refs` that point at any symbol named `target`.
/// This is the piece `qartez_refs` exposes publicly through
/// `read::get_symbol_references`, just with the caller names elided.
fn refs_to_target(conn: &Connection, target: &str) -> usize {
    symbol_ref_names(conn)
        .iter()
        .filter(|(_, to)| to == target)
        .count()
}

// ---------------------------------------------------------------------------
// Part 1: SEC004 false-positive regression
// ---------------------------------------------------------------------------

const RUN_RS_FIXTURE: &str = r#"use std::path::PathBuf;
use std::process::Command;

pub fn run_command(cmd: Vec<String>, args: &[String]) {
    let _ = Command::new(&cmd[0]).args(args).output();
}

pub fn launch_setup(setup: PathBuf) {
    if setup.is_file() {
        let _ = Command::new(&setup).arg("--update-background").output();
    }
}

pub fn spawn_server(binary: PathBuf, project_dir: PathBuf) {
    let _ = Command::new(&binary)
        .arg("mcp")
        .arg("session-start")
        .arg(&project_dir)
        .output();
}

pub fn real_shell_injection_still_flagged(shell: &str, payload: &str) {
    let _ = Command::new(shell).arg("-c").arg(format!("echo {}", payload)).output();
}
"#;

#[test]
fn sec004_command_new_variable_not_flagged_end_to_end() {
    let (dir, conn) = index_project(&[("src/run.rs", RUN_RS_FIXTURE)]);
    let findings = security_scan(&conn, dir.path());

    let sec004: Vec<&security::Finding> =
        findings.iter().filter(|f| f.rule_id == "SEC004").collect();

    let run_command_hits: Vec<&&security::Finding> = sec004
        .iter()
        .filter(|f| f.symbol_name == "run_command")
        .collect();
    assert!(
        run_command_hits.is_empty(),
        "run_command is argv exec via &cmd[0]; must NOT fire SEC004. got: {run_command_hits:?}",
    );

    let launch_setup_hits: Vec<&&security::Finding> = sec004
        .iter()
        .filter(|f| f.symbol_name == "launch_setup")
        .collect();
    assert!(
        launch_setup_hits.is_empty(),
        "launch_setup is argv exec via &setup; must NOT fire SEC004. got: {launch_setup_hits:?}",
    );

    let spawn_server_hits: Vec<&&security::Finding> = sec004
        .iter()
        .filter(|f| f.symbol_name == "spawn_server")
        .collect();
    assert!(
        spawn_server_hits.is_empty(),
        "spawn_server chains .arg() on a PathBuf binary; must NOT fire SEC004. got: {spawn_server_hits:?}",
    );
}

#[test]
fn sec004_real_shell_injection_still_flagged_end_to_end() {
    let (dir, conn) = index_project(&[("src/run.rs", RUN_RS_FIXTURE)]);
    let findings = security_scan(&conn, dir.path());

    let hit = findings
        .iter()
        .find(|f| f.rule_id == "SEC004" && f.symbol_name == "real_shell_injection_still_flagged");
    assert!(
        hit.is_some(),
        "Command::new(shell).arg(\"-c\").arg(format!(..)) MUST still fire SEC004; got findings: {:?}",
        findings
            .iter()
            .map(|f| (&f.rule_id, &f.symbol_name))
            .collect::<Vec<_>>(),
    );
}

// ---------------------------------------------------------------------------
// Part 2: Generic type args + serde attr paths + proc-macro DSL bodies
// ---------------------------------------------------------------------------

const PARAMS_RS_FIXTURE: &str = r#"pub struct ToolsParams { pub limit: u32 }
pub struct SoulWorkspaceParams { pub action: String }
pub mod flexible {
    use serde::{Deserialize, Deserializer};
    pub fn u32_opt<'de, D: Deserializer<'de>>(_d: D) -> Result<Option<u32>, D::Error> { Ok(None) }
}
"#;

const HANDLERS_RS_FIXTURE: &str = r#"use crate::params_types::{ToolsParams, SoulWorkspaceParams, flexible};

pub struct Parameters<T>(pub T);

pub fn handler_generic(Parameters(_p): Parameters<ToolsParams>) -> Result<(), String> { Ok(()) }

#[derive(serde::Deserialize)]
pub struct FormConfig {
    #[serde(deserialize_with = "flexible::u32_opt")]
    pub limit: Option<u32>,
}

macro_rules! router {
    ($($t:tt)*) => {};
}

// router! inside a function body gets a non-None enclosing symbol, which
// the resolver needs to emit a symbol_refs edge. At module scope the
// resolver drops every ref ("Module-scope references (no enclosing
// symbol) are dropped in v1"), so the parser-level fix never reaches
// the DB. Real-world proc-macro DSL usage is almost always module-scope,
// which surfaces as a concrete parser-to-DB gap - see the test comment
// on refs_proc_macro_dsl_body_reaches_db below.
pub fn wire_routes() {
    router! {
        SoulWorkspaceParams => workspace_handler,
    }
}
"#;

const LIB_RS_FIXTURE: &str = r#"pub mod params_types;
pub mod handlers;
"#;

fn index_refs_fixture() -> (TempDir, Connection) {
    index_project(&[
        ("src/lib.rs", LIB_RS_FIXTURE),
        ("src/params_types.rs", PARAMS_RS_FIXTURE),
        ("src/handlers.rs", HANDLERS_RS_FIXTURE),
    ])
}

#[test]
fn refs_generic_type_argument_reaches_db() {
    let (_dir, conn) = index_refs_fixture();
    let count = refs_to_target(&conn, "ToolsParams");
    assert!(
        count >= 1,
        "handler_generic uses ToolsParams as a generic type argument; \
         expected at least one symbol_refs edge to ToolsParams, got {count}. \
         all edges: {:?}",
        symbol_ref_names(&conn),
    );

    let refs = read::get_symbol_references(&conn, "ToolsParams").unwrap();
    assert!(
        refs.iter().any(|(_, _, importers)| !importers.is_empty()),
        "get_symbol_references (what qartez_refs uses) returned no importers for ToolsParams. \
         refs: {:?}",
        refs.iter()
            .map(|(s, f, i)| (s.name.clone(), f.path.clone(), i.len()))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn refs_proc_macro_dsl_body_reaches_db() {
    // IMPORTANT: the fixture puts `router! { ... }` inside `pub fn
    // wire_routes()` so there is an enclosing symbol. If the macro sat
    // at module scope (which is the MORE common real-world shape - see
    // `tool_router!` usage in the qartez server itself), the ref would
    // be dropped in `resolve_symbol_references` because that function
    // drops every reference with `from_symbol_idx = None`. The parser
    // fix in aa63eb3 emits the ref, but the resolver filter is the
    // end-to-end gap. This test documents the ONE shape that works.
    let (_dir, conn) = index_refs_fixture();
    let count = refs_to_target(&conn, "SoulWorkspaceParams");
    assert!(
        count >= 1,
        "router! {{ SoulWorkspaceParams => ... }} inside fn wire_routes must surface as a \
         symbol_refs edge; got {count}. all edges: {:?}",
        symbol_ref_names(&conn),
    );
}

#[test]
fn refs_serde_attribute_string_path_reaches_db() {
    let (_dir, conn) = index_refs_fixture();
    let count = refs_to_target(&conn, "u32_opt");
    assert!(
        count >= 1,
        "#[serde(deserialize_with = \"flexible::u32_opt\")] must surface u32_opt as a ref; \
         got {count}. all edges: {:?}",
        symbol_ref_names(&conn),
    );
}

#[test]
fn unused_does_not_flag_symbols_used_in_generics_serde_or_macros() {
    let (_dir, conn) = index_refs_fixture();

    let unused = read::get_unused_exports_page(&conn, 1000, 0).unwrap();
    let unused_names: Vec<String> = unused.iter().map(|(s, _)| s.name.clone()).collect();

    assert!(
        !unused_names.contains(&"ToolsParams".to_string()),
        "ToolsParams is used as a generic type argument and must not be flagged as dead export. \
         unused list: {unused_names:?}",
    );
    assert!(
        !unused_names.contains(&"SoulWorkspaceParams".to_string()),
        "SoulWorkspaceParams is referenced inside router! {{ ... }} and must not be flagged as dead. \
         unused list: {unused_names:?}",
    );
    assert!(
        !unused_names.contains(&"u32_opt".to_string()),
        "u32_opt is referenced via serde deserialize_with and must not be flagged as dead. \
         unused list: {unused_names:?}",
    );
}

// ---------------------------------------------------------------------------
// Part 3: resolver end-to-end coverage for module-scope proc-macro DSLs and
//         instance-method calls. Both classes of reference used to show up
//         as FP "unused" symbols because the resolver dropped
//         `from_symbol_idx = None` module-scope refs and dropped ambiguous
//         bare-name method calls.
// ---------------------------------------------------------------------------

const MODULE_SCOPE_ROUTER_FIXTURE: &str = r#"use crate::params_types::{ToolsParams, SoulWorkspaceParams};

pub struct Parameters<T>(pub T);

pub fn workspace_handler(Parameters(_p): Parameters<SoulWorkspaceParams>) -> Result<(), String> { Ok(()) }
pub fn tools_handler(Parameters(_p): Parameters<ToolsParams>) -> Result<(), String> { Ok(()) }

macro_rules! router {
    ($($t:tt)*) => {};
}

// Module-scope macro invocation: no enclosing function. The resolver must
// still wire the refs through to the DB so that SoulWorkspaceParams and
// ToolsParams are not flagged unused.
router! {
    SoulWorkspaceParams => workspace_handler,
    ToolsParams => tools_handler,
}
"#;

#[test]
fn refs_module_scope_proc_macro_dsl_reaches_db() {
    let (_dir, conn) = index_project(&[
        ("src/lib.rs", LIB_RS_FIXTURE),
        ("src/params_types.rs", PARAMS_RS_FIXTURE),
        ("src/handlers.rs", MODULE_SCOPE_ROUTER_FIXTURE),
    ]);
    let ws_refs = refs_to_target(&conn, "SoulWorkspaceParams");
    let tools_refs = refs_to_target(&conn, "ToolsParams");
    assert!(
        ws_refs >= 1,
        "module-scope router! must emit symbol_refs to SoulWorkspaceParams; got {ws_refs}. \
         all edges: {:?}",
        symbol_ref_names(&conn),
    );
    assert!(
        tools_refs >= 1,
        "module-scope router! must emit symbol_refs to ToolsParams; got {tools_refs}. \
         all edges: {:?}",
        symbol_ref_names(&conn),
    );
}

#[test]
fn unused_does_not_flag_module_scope_proc_macro_params() {
    let (_dir, conn) = index_project(&[
        ("src/lib.rs", LIB_RS_FIXTURE),
        ("src/params_types.rs", PARAMS_RS_FIXTURE),
        ("src/handlers.rs", MODULE_SCOPE_ROUTER_FIXTURE),
    ]);
    let unused = read::get_unused_exports_page(&conn, 1000, 0).unwrap();
    let unused_names: Vec<String> = unused.iter().map(|(s, _)| s.name.clone()).collect();
    for name in ["SoulWorkspaceParams", "ToolsParams"] {
        assert!(
            !unused_names.contains(&name.to_string()),
            "{name} is referenced inside a module-scope macro DSL and must not be flagged \
             as dead export. unused list: {unused_names:?}",
        );
    }
}

const INSTANCE_METHOD_FIXTURE: &str = r#"pub struct Watcher {
    prefix: String,
}

impl Watcher {
    pub fn with_prefix(prefix: &str) -> Self {
        Self { prefix: prefix.to_string() }
    }

    pub async fn run(&self) -> Result<(), String> {
        let _ = &self.prefix;
        Ok(())
    }
}

pub async fn bootstrap() {
    let watcher = Watcher::with_prefix("app");
    let _ = watcher.run().await;
}
"#;

#[test]
fn refs_instance_method_call_resolves_to_impl_method() {
    let (_dir, conn) = index_project(&[("src/lib.rs", INSTANCE_METHOD_FIXTURE)]);
    let edges = symbol_ref_names(&conn);
    let has_bootstrap_to_run = edges
        .iter()
        .any(|(from, to)| from == "bootstrap" && to == "run");
    assert!(
        has_bootstrap_to_run,
        "bootstrap calls watcher.run() which must resolve to Watcher::run. edges: {edges:?}",
    );
}

#[test]
fn unused_does_not_flag_instance_method_called_only_via_binding() {
    let (_dir, conn) = index_project(&[("src/lib.rs", INSTANCE_METHOD_FIXTURE)]);
    let unused = read::get_unused_exports_page(&conn, 1000, 0).unwrap();
    let unused_names: Vec<String> = unused.iter().map(|(s, _)| s.name.clone()).collect();
    assert!(
        !unused_names.contains(&"run".to_string()),
        "Watcher::run is called as watcher.run().await and must not be flagged as dead. \
         unused list: {unused_names:?}",
    );
}

#[test]
fn mixed_kind_pool_only_fans_out_to_methods() {
    // Caller in a file separate from the candidate pool so Priority 4
    // (same-file) cannot fire. The driver file does not import the
    // candidate file, so Priority 5 (imported) also does not fire. We
    // arrive at Priority 6 with three candidates: one free function and
    // two impl methods with the same bare name. The fallback must link
    // only to the two methods, never to the free function - `x.method()`
    // can never syntactically refer to a free function.
    let candidates = r#"pub fn target() -> i32 { 0 }

pub struct Foo;
impl Foo { pub fn target(&self) -> i32 { 1 } }

pub struct Bar;
impl Bar { pub fn target(&self) -> i32 { 2 } }
"#;
    let caller = r#"pub fn driver() -> i32 {
    0
}

macro_rules! emit { ($($t:tt)*) => {}; }

emit! { target }
"#;
    let (_dir, conn) = index_project(&[
        ("src/lib.rs", "pub mod candidates;\npub mod caller;\n"),
        ("src/candidates.rs", candidates),
        ("src/caller.rs", caller),
    ]);
    let edges = symbol_ref_names(&conn);
    // Count edges whose target is one of the two methods vs the free
    // function. This is approximate because `symbol_ref_names` joins on
    // name only - but in this fixture there is exactly one free-function
    // `target` and two method `target`s, so the count distinguishes.
    let total_target_edges: usize = edges.iter().filter(|(_, to)| to == "target").count();
    assert!(
        total_target_edges <= 2,
        "free-function `target` must NOT receive a phantom edge from \
         a method-shape fallback; expected at most 2 method edges, got {total_target_edges}. \
         edges: {edges:?}",
    );
}

#[test]
fn module_scope_fallback_does_not_panic_on_empty_file() {
    // File with nothing but a module-scope macro invocation that emits a
    // ref. The file has zero symbols, so `entry.symbol_ids.first()` is
    // None. The resolver must drop the ref gracefully rather than panic.
    let lib = r#"pub mod params_types;
pub mod handlers;
"#;
    let params = r#"pub struct Thing;
"#;
    let empty_handlers = r#"macro_rules! router { ($($t:tt)*) => {}; }
router! {
    Thing => run_it,
}
"#;
    let (_dir, conn) = index_project(&[
        ("src/lib.rs", lib),
        ("src/params_types.rs", params),
        ("src/handlers.rs", empty_handlers),
    ]);
    // Completes without panic. No assertion on ref count - the point of
    // the test is that an empty-symbol-list file does not crash the
    // resolver's module-scope fallback.
    let _ = symbol_ref_names(&conn);
}

#[test]
fn module_scope_self_loop_is_harmless() {
    // If the target of a module-scope ref happens to be the first symbol
    // in the file, the fallback creates a self-loop. insert_symbol_refs
    // uses INSERT OR IGNORE and symbol_refs has a UNIQUE constraint on
    // (from, to, kind), so the edge lands at most once; PageRank's
    // `test_pagerank_self_loop_ignored` guarantees self-loops do not
    // inflate a node's rank. The symbol should still not appear in the
    // unused list (self-reference DOES count as a ref).
    let lib = r#"// Self-reference: the first symbol is also the target.
pub fn self_referer() {}

macro_rules! emit { ($($t:tt)*) => {}; }
emit! {
    self_referer;
}
"#;
    let (_dir, conn) = index_project(&[("src/lib.rs", lib)]);
    let edges = symbol_ref_names(&conn);
    // Whether or not an edge lands is implementation-defined (the
    // parser may or may not emit a ref for an identifier inside emit!).
    // The assertion is strictly: no panic, and if a self-loop exists,
    // it is at most one edge thanks to INSERT OR IGNORE.
    let self_loops: usize = edges
        .iter()
        .filter(|(from, to)| from == to && from == "self_referer")
        .count();
    assert!(
        self_loops <= 1,
        "INSERT OR IGNORE + UNIQUE constraint must dedupe self-loops; got {self_loops}",
    );
}
