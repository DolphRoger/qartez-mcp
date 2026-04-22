// Rust guideline compliant 2026-04-22
//
// End-to-end regression tests for the analyzer fix in commit c309e1d:
// verifies that through the full index-and-query pipeline,
//   1. clone detection excludes `#[cfg(test)]` modules and `tests/` paths
//      by default but restores them under `include_tests=true`;
//   2. cyclomatic complexity no longer inflates on `?` operators and that
//      feature-envy skips associated-function calls while still flagging
//      instance-method envy;
//   3. against qartez-public's own source, the originally-misflagged
//      symbols (SEC004 false-positives and proc-macro-DSL / serde
//      deserialize_with identifiers) no longer surface.

use std::fs;
use std::path::Path;

use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

use qartez_mcp::index;
use qartez_mcp::server::QartezServer;
use qartez_mcp::storage::{read, schema};

// ---------------------------------------------------------------------------
// Shared fixture helpers
// ---------------------------------------------------------------------------

fn setup_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    schema::create_schema(&conn).unwrap();
    conn
}

fn build_and_index(dir: &Path) -> QartezServer {
    // Simulate a project: a `.git` dir marks it as a project, so downstream
    // tools treat the TempDir as the project root.
    fs::create_dir_all(dir.join(".git")).unwrap();
    let conn = setup_db();
    index::full_index(&conn, dir, false).unwrap();
    QartezServer::new(conn, dir.to_path_buf(), 0)
}

// ---------------------------------------------------------------------------
// Part 1. Clone detector excludes test modules by default
// ---------------------------------------------------------------------------

fn write_clones_fixture(dir: &Path) {
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    // Two production-side clones (process_a / process_b) plus a `#[cfg(test)]`
    // module with two structurally identical fixtures (test_fixture_alpha /
    // test_fixture_beta). Default scan must drop the cfg(test) members.
    let main_lib = r#"pub fn process_a(items: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for x in &items { if x.len() > 3 { out.push(x.clone()); } }
    out.sort();
    out.dedup();
    out
}

pub fn process_b(items: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for x in &items { if x.len() > 3 { out.push(x.clone()); } }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_fixture_alpha(items: Vec<String>) -> Vec<String> {
        let mut out = Vec::new();
        for x in &items { if x.len() > 3 { out.push(x.clone()); } }
        out.sort();
        out.dedup();
        out
    }

    fn test_fixture_beta(items: Vec<String>) -> Vec<String> {
        let mut out = Vec::new();
        for x in &items { if x.len() > 3 { out.push(x.clone()); } }
        out.sort();
        out.dedup();
        out
    }
}
"#;
    fs::write(src.join("main_lib.rs"), main_lib).unwrap();

    // Test-path file: same shape, at a conventional test path.
    let tests = dir.join("tests");
    fs::create_dir_all(&tests).unwrap();
    let integration = r#"pub fn integration_helper(items: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for x in &items { if x.len() > 3 { out.push(x.clone()); } }
    out.sort();
    out.dedup();
    out
}
"#;
    fs::write(tests.join("integration.rs"), integration).unwrap();
}

#[test]
fn clones_default_excludes_cfg_test_and_test_path_members() {
    let dir = TempDir::new().unwrap();
    write_clones_fixture(dir.path());
    let server = build_and_index(dir.path());

    // Default scan - `include_tests` omitted.
    let out = server
        .call_tool_by_name(
            "qartez_clones",
            json!({ "min_lines": 5, "limit": 50, "format": "detailed" }),
        )
        .expect("qartez_clones default should succeed");

    assert!(
        out.contains("process_a") && out.contains("process_b"),
        "production clones must surface by default: {out}"
    );
    assert!(
        !out.contains("test_fixture_alpha"),
        "cfg(test) fixture alpha must be filtered by default: {out}"
    );
    assert!(
        !out.contains("test_fixture_beta"),
        "cfg(test) fixture beta must be filtered by default: {out}"
    );
    assert!(
        !out.contains("integration_helper"),
        "tests/ path member must be filtered by default: {out}"
    );
}

#[test]
fn clones_include_tests_restores_all_members() {
    let dir = TempDir::new().unwrap();
    write_clones_fixture(dir.path());
    let server = build_and_index(dir.path());

    let out = server
        .call_tool_by_name(
            "qartez_clones",
            json!({
                "min_lines": 5,
                "limit": 50,
                "include_tests": true,
                "format": "detailed",
            }),
        )
        .expect("qartez_clones with include_tests=true should succeed");

    for expected in [
        "process_a",
        "process_b",
        "test_fixture_alpha",
        "test_fixture_beta",
        "integration_helper",
    ] {
        assert!(
            out.contains(expected),
            "include_tests=true must surface '{expected}': {out}"
        );
    }
}

// ---------------------------------------------------------------------------
// Part 2. CC ignores `?` and feature-envy skips associated calls
// ---------------------------------------------------------------------------

fn write_dispatcher_fixture(dir: &Path) {
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    // Flat 8-arm dispatcher: each arm uses `?` for error propagation. With
    // the fix, `?` contributes 0 to CC, so CC tracks the number of match
    // arms (plus the outer match node itself, depending on tree-sitter
    // accounting). Before the fix, CC would inflate to roughly 25+.
    //
    // `construct_pipeline` calls only `Step {}` literal constructors and
    // the associated function style is not present here - we rely on the
    // absence of envy signal. `envy_bar` calls `&self` instance methods
    // four times on a foreign type; that is genuine feature envy.
    let dispatcher = r#"use std::io::{Read, Write};

pub struct Runner;

pub struct Step;

impl Step {
    pub fn build_a(&self) -> Result<(), String> { Ok(()) }
    pub fn build_b(&self) -> Result<(), String> { Ok(()) }
    pub fn build_c(&self) -> Result<(), String> { Ok(()) }
    pub fn build_d(&self) -> Result<(), String> { Ok(()) }
    pub fn factory() -> Step { Step {} }
}

impl Runner {
    pub fn run(&self, op: &str) -> Result<(), String> {
        match op {
            "a" => { let _ = std::fs::read("/tmp/a").map_err(|e| e.to_string())?; Ok(()) }
            "b" => { let _ = std::fs::read("/tmp/b").map_err(|e| e.to_string())?; Ok(()) }
            "c" => { let _ = std::fs::read("/tmp/c").map_err(|e| e.to_string())?; Ok(()) }
            "d" => { let _ = std::fs::read("/tmp/d").map_err(|e| e.to_string())?; Ok(()) }
            "e" => { let _ = std::fs::read("/tmp/e").map_err(|e| e.to_string())?; Ok(()) }
            "f" => { let _ = std::fs::read("/tmp/f").map_err(|e| e.to_string())?; Ok(()) }
            "g" => { let _ = std::fs::read("/tmp/g").map_err(|e| e.to_string())?; Ok(()) }
            "h" => { let _ = std::fs::read("/tmp/h").map_err(|e| e.to_string())?; Ok(()) }
            _ => Ok(()),
        }
    }

    pub fn construct_pipeline(&self) -> Vec<Step> {
        let a = Step::factory();
        let b = Step::factory();
        let c = Step::factory();
        let d = Step::factory();
        let e = Step::factory();
        vec![a, b, c, d, e]
    }

    pub fn envy_bar(&self, bar: &Step) -> Result<(), String> {
        bar.build_a()?;
        bar.build_b()?;
        bar.build_c()?;
        bar.build_d()
    }
}
"#;
    fs::write(src.join("dispatcher.rs"), dispatcher).unwrap();
}

/// Fetch the complexity value for a symbol by (name, file_path). Returns
/// the first matching row or None when the symbol was not indexed.
fn complexity_for(conn: &Connection, name: &str, file_path: &str) -> Option<u32> {
    let all = read::get_all_symbols_with_path(conn).unwrap();
    all.into_iter()
        .find(|(s, p)| s.name == name && p == file_path)
        .and_then(|(s, _)| s.complexity)
}

#[test]
fn cc_runner_run_does_not_count_try_operator() {
    let dir = TempDir::new().unwrap();
    write_dispatcher_fixture(dir.path());
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    let conn = setup_db();
    index::full_index(&conn, dir.path(), false).unwrap();

    let cc = complexity_for(&conn, "run", "src/dispatcher.rs")
        .expect("Runner::run must be indexed with a complexity value");

    // With the fix, `?` adds 0. 8 match arms plus the outer match fallback
    // is well under 15. Before the fix, per-arm `?` pushed CC above 20.
    assert!(
        cc < 15,
        "Runner::run CC expected < 15 after the `?` fix, got {cc}"
    );
    // Sanity lower bound: there are still at least 8 match arms, so CC
    // must be at least 8.
    assert!(
        cc >= 8,
        "Runner::run CC expected >= 8 (8 match arms), got {cc}"
    );
}

#[test]
fn feature_envy_skips_associated_calls_but_flags_instance_calls() {
    let dir = TempDir::new().unwrap();
    write_dispatcher_fixture(dir.path());
    let server = build_and_index(dir.path());

    // Low envy_ratio so any instance-method envy surfaces. We explicitly
    // request only feature_envy to keep the output focused.
    let out = server
        .call_tool_by_name(
            "qartez_smells",
            json!({
                "kind": "feature_envy",
                "envy_ratio": 1.0,
                "limit": 50,
                "format": "detailed",
            }),
        )
        .expect("qartez_smells feature_envy should succeed");

    // `construct_pipeline` calls `Step::factory()` repeatedly, an
    // associated function with no `self` receiver. The fix excludes
    // associated-function calls from envy accounting, so the symbol must
    // NOT appear in the feature-envy section.
    assert!(
        !out.contains("construct_pipeline"),
        "construct_pipeline must not be flagged as feature envy: {out}"
    );

    // `envy_bar` calls four `&self` instance methods on `Step` from a
    // method whose own_type is `Runner`. That is real feature envy and
    // must still be flagged with envied_type=Step.
    assert!(
        out.contains("envy_bar"),
        "envy_bar must still be flagged as feature envy: {out}"
    );
    assert!(
        out.contains("Step"),
        "feature-envy output must name the envied type Step: {out}"
    );
}

// ---------------------------------------------------------------------------
// Part 3. Self-test: qartez analyzers against qartez-public's own source
//
// A full self-index of qartez-public runs in < 1s in release mode (CI
// builds with `--release`), so these regressions run on every PR rather
// than hiding behind `#[ignore]`. They catch the class of false positives
// that stale-binary installs routinely regressed on.
// ---------------------------------------------------------------------------

/// Resolve the qartez-public source directory relative to the Cargo
/// manifest. `CARGO_MANIFEST_DIR` is set by cargo to the directory of the
/// crate being tested, which is qartez-public itself.
fn qartez_public_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn selftest_sec004_no_false_positives_on_qartez_public() {
    let root = qartez_public_root();
    assert!(
        root.join("src/lib.rs").exists(),
        "qartez-public/src/lib.rs must exist at {root:?}",
    );
    let conn = setup_db();
    index::full_index(&conn, &root, false).unwrap();
    let server = QartezServer::new(conn, root, 0);

    let out = server
        .call_tool_by_name(
            "qartez_security",
            json!({ "limit": 500, "format": "concise" }),
        )
        .expect("qartez_security on self must succeed");

    // Regression guard: these three functions were SEC004 FPs before the
    // commit-c309e1d fix. They must no longer appear in SEC004 findings.
    for (symbol, file) in &[
        ("run_command", "src/toolchain.rs"),
        ("schedule_update_check", "src/main.rs"),
        ("run_session_start", "src/bin/setup.rs"),
    ] {
        // SEC004 is the Command/process-injection rule. A line containing
        // both "SEC004" and the symbol name is the strongest signal we
        // can check from the concise one-line-per-finding output.
        let flagged = out.lines().any(|l| {
            l.contains("SEC004") && l.contains(symbol) && (l.contains(file) || l.contains(".rs"))
        });
        assert!(
            !flagged,
            "{symbol} (in {file}) must NOT appear in SEC004 findings after the fix:\n{out}"
        );
    }
}

#[test]
fn selftest_unused_no_false_positives_on_qartez_public() {
    let root = qartez_public_root();
    assert!(
        root.join("src/lib.rs").exists(),
        "qartez-public/src/lib.rs must exist at {root:?}",
    );
    let conn = setup_db();
    index::full_index(&conn, &root, false).unwrap();
    let server = QartezServer::new(conn, root, 0);

    // Ask for a very large page so every unused export is returned in one
    // response; then scan the text for specific symbol names.
    let out = server
        .call_tool_by_name("qartez_unused", json!({ "limit": 5000 }))
        .expect("qartez_unused on self must succeed");

    // Proc-macro DSL parameter structs - referenced from the
    // `dispatch_tool_call!` macro body in `QartezServer::call_tool_by_name`.
    // Before the fix, the rust_lang parser did not descend into token trees
    // of non-builtin macros, so uppercase identifiers there never emitted
    // ref edges and these structs were reported as unused exports.
    //
    // `ToolsParams` is referenced via `Parameters<ToolsParams>` inside a
    // `#[tool]`-attributed async method signature. The follow-up fix in
    // `extract_impl_methods` walks the full method node (not just the body),
    // so signature type references in `impl` methods now reach the resolver.
    for name in &[
        "SoulWorkspaceParams",
        "SoulSecurityParams",
        "SoulHierarchyParams",
        "ToolsParams",
    ] {
        assert!(
            !contains_symbol_line(&out, name),
            "{name} must NOT be reported as unused after the fix (proc-macro-DSL ref):\n{out}"
        );
    }

    // `deserialize_with = "flexible::u32_opt"` - the rust_lang parser now
    // parses the string path inside the serde attribute and emits a Use
    // ref to the tail segment.
    for name in &["u32_opt", "bool_opt", "f64_opt"] {
        assert!(
            !contains_symbol_line(&out, name),
            "{name} must NOT be reported as unused after the fix (serde deserialize_with):\n{out}"
        );
    }

    // MCP tool methods wired via rmcp's `#[tool_router]` proc macro. The
    // generated dispatch surface is invisible to the static import graph,
    // so without the `unused_excluded` stamp applied by the rust_lang
    // parser every one of these showed up as dead code on a self-scan.
    // Cover each router entry in `server/tools/mod.rs::tool_router` that
    // previously surfaced as an FP.
    for name in &[
        "qartez_map",
        "qartez_security",
        "qartez_workspace",
        "qartez_semantic",
        "qartez_tools",
        "qartez_hierarchy",
        "qartez_insert_before_symbol",
        "qartez_insert_after_symbol",
        "qartez_replace_symbol",
        "qartez_safe_delete",
    ] {
        assert!(
            !contains_symbol_line(&out, name),
            "{name} must NOT be reported as unused (wired via #[tool_router]):\n{out}"
        );
    }

    // Regression for the macro-body Call-walker: `Severity::label` is
    // invoked as `f.severity.label()` inside `format!(...)` calls in
    // `qartez_security`. Before the fix, the token-tree walker skipped
    // builtin macros and never emitted the Call ref, so the method read
    // as dead.
    assert!(
        !contains_symbol_line(&out, "label"),
        "`label` method on Severity must NOT be unused (called in format!() bodies):\n{out}"
    );
}

/// Heuristic whole-word match for a symbol in the `qartez_unused` output.
/// The compact format is `<kind-letter> <name> L<line>` per entry; we
/// search for ` <name> L` to avoid matching prefixes inside longer names.
fn contains_symbol_line(out: &str, name: &str) -> bool {
    let pat = format!(" {name} L");
    out.lines().any(|l| l.contains(&pat))
}

// ---------------------------------------------------------------------------
// Part 4. Clone detector preserves string-literal bodies on data declarations.
// Before this fix the shape-hasher collapsed every string literal to `_S`,
// so `const A: &str = "..."; const B: &str = "..."` hashed identically
// regardless of body. That swept ~12 of the 13 CREATE_* SQL schema consts
// into a single clone group.
// ---------------------------------------------------------------------------

#[test]
fn clones_does_not_collapse_distinct_const_string_literals() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    let schema = r#"pub const CREATE_FOO: &str = "
CREATE TABLE IF NOT EXISTS foo (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    ts INTEGER NOT NULL
)";

pub const CREATE_BAR: &str = "
CREATE TABLE IF NOT EXISTS bar (
    file_id INTEGER NOT NULL,
    count INTEGER NOT NULL,
    last_seen INTEGER
)";

pub const CREATE_BAZ: &str = "
CREATE TABLE IF NOT EXISTS baz (
    id TEXT PRIMARY KEY,
    value REAL NOT NULL,
    created_at INTEGER NOT NULL
)";
"#;
    fs::write(src.join("schema.rs"), schema).unwrap();

    let server = build_and_index(dir.path());
    let out = server
        .call_tool_by_name(
            "qartez_clones",
            json!({ "min_lines": 3, "limit": 50, "format": "detailed" }),
        )
        .expect("qartez_clones should succeed");

    for name in ["CREATE_FOO", "CREATE_BAR", "CREATE_BAZ"] {
        assert!(
            !out.contains(name),
            "{name} must not be grouped as a clone just because all three are \
             `const X: &str = \"...different SQL...\"`. output:\n{out}"
        );
    }
}
