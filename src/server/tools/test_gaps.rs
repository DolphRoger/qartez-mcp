#![allow(unused_imports)]

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ErrorData};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, tool, tool_router};

use super::super::QartezServer;
use super::super::helpers::{self, *};
use super::super::params::*;
use super::super::tiers;
use super::super::treesitter::*;

use crate::graph::blast;
use crate::guard;
use crate::storage::read;
use crate::storage::read::sanitize_fts_query;
use crate::toolchain;

#[tool_router(router = qartez_test_gaps_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_test_gaps",
        description = "Test-to-code mapping, coverage gap detection, and test suggestion for changes. Three modes: 'map' shows which test files cover which source files via import edges. 'gaps' (default) finds untested source files ranked by risk score (health * blast radius). 'suggest' takes a git diff range and returns which existing tests to run plus which changed files lack test coverage.",
        annotations(
            title = "Test Coverage Gaps",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(in crate::server) fn qartez_test_gaps(
        &self,
        Parameters(params): Parameters<SoulTestGapsParams>,
    ) -> Result<String, String> {
        let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
        let limit = params.limit.unwrap_or(30) as usize;
        let concise = is_concise(&params.format);
        let mode = params.mode.as_deref().unwrap_or("gaps");

        let all_files = read::get_all_files(&conn).map_err(|e| format!("DB error: {e}"))?;
        let all_edges = read::get_all_edges(&conn).map_err(|e| format!("DB error: {e}"))?;
        let ctx = TestGapsCtx::build(&all_files, &all_edges);

        match mode {
            "map" => self.test_gaps_map(&params, &ctx, &conn, limit, concise),
            "gaps" => self.test_gaps_find(&params, &ctx, &conn, limit, concise),
            "suggest" => self.test_gaps_suggest(&params, &ctx, &conn, limit, concise),
            _ => Err(format!(
                "Unknown mode '{mode}'. Use 'map', 'gaps', or 'suggest'."
            )),
        }
    }
}

struct TestGapsCtx<'a> {
    all_files: &'a [crate::storage::models::FileRow],
    id_to_file: HashMap<i64, &'a crate::storage::models::FileRow>,
    path_to_id: HashMap<&'a str, i64>,
    forward: HashMap<i64, Vec<i64>>,
    reverse: HashMap<i64, Vec<i64>>,
}

impl<'a> TestGapsCtx<'a> {
    fn build(all_files: &'a [crate::storage::models::FileRow], all_edges: &[(i64, i64)]) -> Self {
        let id_to_file: HashMap<i64, &'a crate::storage::models::FileRow> =
            all_files.iter().map(|f| (f.id, f)).collect();
        let path_to_id: HashMap<&'a str, i64> =
            all_files.iter().map(|f| (f.path.as_str(), f.id)).collect();

        let mut forward: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut reverse: HashMap<i64, Vec<i64>> = HashMap::new();
        for &(from, to) in all_edges {
            if from != to {
                forward.entry(from).or_default().push(to);
                reverse.entry(to).or_default().push(from);
            }
        }

        Self {
            all_files,
            id_to_file,
            path_to_id,
            forward,
            reverse,
        }
    }
}

impl QartezServer {
    fn test_gaps_map(
        &self,
        params: &SoulTestGapsParams,
        ctx: &TestGapsCtx<'_>,
        conn: &rusqlite::Connection,
        limit: usize,
        concise: bool,
    ) -> Result<String, String> {
        let mut source_to_tests: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut test_to_sources: HashMap<&str, Vec<&str>> = HashMap::new();

        for file in ctx.all_files {
            if !is_test_path(&file.path) {
                continue;
            }
            let imports = ctx.forward.get(&file.id).cloned().unwrap_or_default();
            for imp_id in imports {
                if let Some(imp_file) = ctx.id_to_file.get(&imp_id)
                    && !is_test_path(&imp_file.path)
                {
                    source_to_tests
                        .entry(imp_file.path.as_str())
                        .or_default()
                        .push(file.path.as_str());
                    test_to_sources
                        .entry(file.path.as_str())
                        .or_default()
                        .push(imp_file.path.as_str());
                }
            }
        }

        if let Some(ref fp) = params.file_path {
            let resolved = self.safe_resolve(fp).map_err(|e| e.to_string())?;
            let rel = crate::index::to_forward_slash(
                resolved
                    .strip_prefix(&self.project_root)
                    .unwrap_or(&resolved)
                    .to_string_lossy()
                    .into_owned(),
            );

            if is_test_path(&rel) {
                let sources = test_to_sources
                    .get(rel.as_str())
                    .cloned()
                    .unwrap_or_default();
                if sources.is_empty() {
                    return Ok(format!("Test file '{rel}' has no indexed source imports."));
                }
                let mut out = format!(
                    "# Test coverage: {rel}\n\nImports {} source file(s):\n",
                    sources.len(),
                );
                for src in sources.iter().take(limit) {
                    out.push_str(&format!("  - {src}\n"));
                }
                return Ok(out);
            }

            let tests = source_to_tests
                .get(rel.as_str())
                .cloned()
                .unwrap_or_default();
            if tests.is_empty() {
                return Ok(format!(
                    "Source file '{rel}' has no test files importing it."
                ));
            }
            let mut out = format!("# Test coverage: {rel}\n\n{} test file(s):\n", tests.len(),);
            for t in tests.iter().take(limit) {
                out.push_str(&format!("  - {t}\n"));
            }
            if params.include_symbols.unwrap_or(false)
                && let Some(&file_id) = ctx.path_to_id.get(rel.as_str())
            {
                let symbols = read::get_symbols_for_file(conn, file_id)
                    .map_err(|e| format!("DB error: {e}"))?;
                let exported: Vec<_> = symbols.iter().filter(|s| s.is_exported).collect();
                if !exported.is_empty() {
                    out.push_str(&format!("\n{} exported symbols:\n", exported.len(),));
                    for sym in exported.iter().take(20) {
                        out.push_str(&format!("  - {} ({})\n", sym.name, sym.kind));
                    }
                }
            }
            return Ok(out);
        }

        let mut entries: Vec<(&str, &Vec<&str>)> =
            source_to_tests.iter().map(|(&k, v)| (k, v)).collect();
        entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

        let total_covered = entries.len();
        let total_source = ctx
            .all_files
            .iter()
            .filter(|f| !is_test_path(&f.path))
            .count();
        let total_test = ctx
            .all_files
            .iter()
            .filter(|f| is_test_path(&f.path))
            .count();

        let mut out = format!(
            "# Test-to-source mapping\n\n{total_covered}/{total_source} source files covered by {total_test} test files\n\n",
        );

        if concise {
            for (src, tests) in entries.iter().take(limit) {
                out.push_str(&format!("  {} ({})\n", src, tests.len()));
            }
        } else {
            for (src, tests) in entries.iter().take(limit) {
                out.push_str(&format!("- {} ({} tests)\n", src, tests.len()));
                for t in tests.iter().take(5) {
                    out.push_str(&format!("    - {t}\n"));
                }
                if tests.len() > 5 {
                    out.push_str(&format!("    ... and {} more\n", tests.len() - 5,));
                }
            }
        }
        if entries.len() > limit {
            out.push_str(&format!(
                "\n... and {} more (use limit= to see more)\n",
                entries.len() - limit,
            ));
        }

        Ok(out)
    }

    fn test_gaps_find(
        &self,
        params: &SoulTestGapsParams,
        ctx: &TestGapsCtx<'_>,
        conn: &rusqlite::Connection,
        limit: usize,
        concise: bool,
    ) -> Result<String, String> {
        let min_pagerank = params.min_pagerank.unwrap_or(0.0);

        let all_syms =
            read::get_all_symbols_with_path(conn).map_err(|e| format!("DB error: {e}"))?;
        let mut max_cc_by_path: HashMap<&str, u32> = HashMap::new();
        for (sym, path) in &all_syms {
            if let Some(cc) = sym.complexity {
                let entry = max_cc_by_path.entry(path.as_str()).or_insert(0);
                if cc > *entry {
                    *entry = cc;
                }
            }
        }

        let health_of = |max_cc: f64, coupling: f64, churn: i64| -> f64 {
            let cc_h = 10.0 / (1.0 + max_cc / 10.0);
            let coupling_h = 10.0 / (1.0 + coupling * 50.0);
            let churn_h = 10.0 / (1.0 + churn as f64 / 8.0);
            (cc_h + coupling_h + churn_h) / 3.0
        };

        let mut gaps: Vec<(&crate::storage::models::FileRow, f64)> = Vec::new();

        for file in ctx.all_files {
            if is_test_path(&file.path) || file.pagerank < min_pagerank {
                continue;
            }

            let has_test_importer = ctx.reverse.get(&file.id).is_some_and(|importers| {
                importers.iter().any(|&imp_id| {
                    ctx.id_to_file
                        .get(&imp_id)
                        .is_some_and(|f| is_test_path(&f.path))
                })
            });

            let covered =
                has_test_importer || has_inline_rust_tests(&self.project_root, &file.path);

            if !covered {
                let max_cc = max_cc_by_path.get(file.path.as_str()).copied().unwrap_or(0) as f64;
                let health = health_of(max_cc, file.pagerank, file.change_count);
                let blast_count = ctx.reverse.get(&file.id).map(|v| v.len()).unwrap_or(0);
                let score = (10.0 - health) * (1.0 + blast_count as f64 / 10.0);
                gaps.push((file, score));
            }
        }

        gaps.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        if gaps.is_empty() {
            return Ok(
                "No untested source files found. All source files are covered by an external test file import or inline Rust tests (`#[cfg(test)]` / `#[test]`)."
                    .to_string(),
            );
        }

        let total_source = ctx
            .all_files
            .iter()
            .filter(|f| !is_test_path(&f.path))
            .count();
        let gap_count = gaps.len();
        let shown = gap_count.min(limit);

        let mut out =
            format!("# Test coverage gaps ({gap_count}/{total_source} source files untested)\n\n",);
        if shown < gap_count {
            out.push_str(&format!(
                "Showing {shown} of {gap_count} (use limit= to see more).\n\n",
            ));
        }

        if concise {
            for (file, score) in gaps.iter().take(limit) {
                let blast = ctx.reverse.get(&file.id).map(|v| v.len()).unwrap_or(0);
                out.push_str(&format!(
                    "  {} PR={:.4} blast={} score={:.1}\n",
                    file.path, file.pagerank, blast, score,
                ));
            }
        } else {
            out.push_str("| File | PageRank | Blast | Churn | Score |\n");
            out.push_str("|------|----------|-------|-------|-------|\n");
            for (file, score) in gaps.iter().take(limit) {
                let blast = ctx.reverse.get(&file.id).map(|v| v.len()).unwrap_or(0);
                out.push_str(&format!(
                    "| {} | {:.4} | {} | {} | {:.1} |\n",
                    truncate_path(&file.path, 40),
                    file.pagerank,
                    blast,
                    file.change_count,
                    score,
                ));
            }
        }

        Ok(out)
    }

    fn test_gaps_suggest(
        &self,
        params: &SoulTestGapsParams,
        ctx: &TestGapsCtx<'_>,
        conn: &rusqlite::Connection,
        limit: usize,
        concise: bool,
    ) -> Result<String, String> {
        let base = params.base.as_deref().ok_or(
            "The 'suggest' mode requires a 'base' parameter (git diff range, e.g., 'main' or 'HEAD~3').",
        )?;

        let changed = crate::git::diff::changed_files_in_range(&self.project_root, base)
            .map_err(|e| format!("Git error: {e}"))?;

        if changed.is_empty() {
            return Ok(format!("No files changed in range '{base}'."));
        }

        let changed_source: Vec<&str> = changed
            .iter()
            .map(|s| s.as_str())
            .filter(|p| !is_test_path(p))
            .collect();
        let changed_tests: Vec<&str> = changed
            .iter()
            .map(|s| s.as_str())
            .filter(|p| is_test_path(p))
            .collect();

        let mut tests_to_run: HashMap<String, Vec<String>> = HashMap::new();
        let mut untested_sources: Vec<&str> = Vec::new();

        for &src_path in &changed_source {
            let file_id = match ctx.path_to_id.get(src_path) {
                Some(&id) => id,
                None => {
                    untested_sources.push(src_path);
                    continue;
                }
            };

            guard::touch_ack(&self.project_root, src_path);

            let mut found_tests: Vec<String> = Vec::new();

            if let Some(importers) = ctx.reverse.get(&file_id) {
                for &imp_id in importers {
                    if let Some(imp_file) = ctx.id_to_file.get(&imp_id)
                        && is_test_path(&imp_file.path)
                    {
                        found_tests.push(imp_file.path.clone());
                    }
                }
            }

            let cochanges = read::get_cochanges(conn, file_id, 10).unwrap_or_default();
            for (_, partner) in &cochanges {
                if is_test_path(&partner.path) && !found_tests.contains(&partner.path) {
                    found_tests.push(partner.path.clone());
                }
            }

            if found_tests.is_empty() && !has_inline_rust_tests(&self.project_root, src_path) {
                untested_sources.push(src_path);
            } else if !found_tests.is_empty() {
                for t in &found_tests {
                    tests_to_run
                        .entry(t.clone())
                        .or_default()
                        .push(src_path.to_string());
                }
            }
        }

        for &test_path in &changed_tests {
            if !tests_to_run.contains_key(test_path) {
                tests_to_run
                    .entry(test_path.to_string())
                    .or_default()
                    .push("(directly changed)".into());
            }
            guard::touch_ack(&self.project_root, test_path);
        }

        let mut test_entries: Vec<(&String, &Vec<String>)> = tests_to_run.iter().collect();
        test_entries.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

        let mut out = format!(
            "# Test suggestion for {base}\n\n{} changed files ({} source, {} test)\n\n",
            changed.len(),
            changed_source.len(),
            changed_tests.len(),
        );

        if test_entries.is_empty() && untested_sources.is_empty() {
            out.push_str("No test files found for the changed source files.\n");
            return Ok(out);
        }

        if !test_entries.is_empty() {
            out.push_str(&format!(
                "## Tests to run ({} test files)\n",
                test_entries.len(),
            ));
            if concise {
                for (test, sources) in test_entries.iter().take(limit) {
                    out.push_str(&format!("  {} (covers {})\n", test, sources.len(),));
                }
            } else {
                for (test, sources) in test_entries.iter().take(limit) {
                    out.push_str(&format!("- {test}\n"));
                    for src in sources.iter().take(5) {
                        out.push_str(&format!("    covers: {src}\n"));
                    }
                    if sources.len() > 5 {
                        out.push_str(&format!("    ... and {} more\n", sources.len() - 5,));
                    }
                }
            }
            out.push('\n');
        }

        if !untested_sources.is_empty() {
            out.push_str(&format!(
                "## Untested changes ({} source files need new tests)\n",
                untested_sources.len(),
            ));
            for src in untested_sources.iter().take(limit) {
                out.push_str(&format!("  - {src}\n"));
            }
        }

        Ok(out)
    }
}
