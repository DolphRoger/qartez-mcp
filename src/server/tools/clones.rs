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

#[tool_router(router = qartez_clones_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_clones",
        description = "Detect duplicate code: groups of symbols with identical structural shape (same AST skeleton after normalizing identifiers, literals, and comments). Each group is a refactoring opportunity — extract the common logic into a shared function. Use min_lines to filter out trivial matches. Test files and inline `#[cfg(test)]` modules are excluded by default (parallel parser-fixture tests share AST shapes on purpose); set `include_tests=true` to scan them too.",
        annotations(
            title = "Code Clone Detection",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(in crate::server) fn qartez_clones(
        &self,
        Parameters(params): Parameters<SoulClonesParams>,
    ) -> Result<String, String> {
        let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
        let limit = params.limit.unwrap_or(20).max(1) as i64;
        let offset = params.offset.unwrap_or(0) as i64;
        // Default raised from 5 to 8 because short dispatch boilerplate
        // (e.g. 37 parallel `fn parse_X(source) -> Tree` helpers that all
        // wrap a tree-sitter parser on a language-specific `LANGUAGE`
        // constant) dominated the top groups without being refactorable -
        // each call site binds a LANGUAGE from a different crate and
        // cannot collapse into a single generic helper without a typeid
        // map. Callers who still want the aggressive cutoff pass
        // `min_lines=5` explicitly.
        let min_lines = params.min_lines.unwrap_or(8);
        let include_tests = params.include_tests.unwrap_or(false);
        let concise = matches!(params.format, Some(Format::Concise));

        let total =
            read::count_clone_groups(&conn, min_lines).map_err(|e| format!("DB error: {e}"))?;
        if total == 0 {
            return Ok(
                "No code clones detected. All symbols have unique structural shapes.".to_string(),
            );
        }

        let groups = read::get_clone_groups(&conn, min_lines, limit, offset)
            .map_err(|e| format!("DB error: {e}"))?;

        // Default behaviour mirrors `qartez_security`: drop symbols whose
        // file path looks like a test file and symbols whose line range
        // sits inside a Rust `#[cfg(test)] mod tests {}` block. Parallel
        // parser-fixture tests (21+ near-identical `test_module` /
        // `test_simple_function` functions in `src/index/languages/*.rs`)
        // are AST-shape-identical by design and dominate the top groups
        // without being refactorable; keep them out of the default view
        // so real production duplicates surface. Pass
        // `include_tests=true` to restore the old behaviour.
        let cfg_test_cache = CfgTestBlockCache::new(&self.project_root);
        let groups: Vec<read::CloneGroup> = if include_tests {
            groups
        } else {
            groups
                .into_iter()
                .filter_map(|g| filter_test_members(g, &cfg_test_cache))
                .collect()
        };

        if groups.is_empty() {
            return Ok(format!(
                "No clones in page (total={total}, offset={offset})."
            ));
        }

        let shown = groups.len() as i64;
        let mut out = if shown < total {
            format!(
                "{total} clone group(s) (min {min_lines} lines); showing {shown} from offset {offset} (next: offset={}).\n\n",
                offset + shown
            )
        } else {
            format!("{total} clone group(s) (min {min_lines} lines).\n\n")
        };

        let total_dup_symbols: usize = groups.iter().map(|g| g.symbols.len()).sum();
        out.push_str(&format!(
            "{total_dup_symbols} duplicate symbols across {shown} group(s).\n\n"
        ));

        for (i, group) in groups.iter().enumerate() {
            let group_num = offset as usize + i + 1;
            let size = group.symbols.len();
            let lines = group
                .symbols
                .first()
                .map(|(s, _)| s.line_end.saturating_sub(s.line_start) + 1)
                .unwrap_or(0);

            if concise {
                out.push_str(&format!("#{group_num} ({size}x, ~{lines}L):"));
                for (sym, file) in &group.symbols {
                    out.push_str(&format!(" {}:{}", file.path, sym.line_start));
                }
                out.push('\n');
            } else {
                out.push_str(&format!(
                    "## Clone group #{group_num} — {size} duplicates, ~{lines} lines each\n"
                ));
                for (sym, file) in &group.symbols {
                    let kind_char = sym.kind.chars().next().unwrap_or(' ');
                    out.push_str(&format!(
                        "  {kind_char} {} @ {} L{}-{}\n",
                        sym.name, file.path, sym.line_start, sym.line_end,
                    ));
                }
                out.push('\n');
            }
        }
        Ok(out)
    }
}

/// Lazy per-file cache of Rust `#[cfg(test)]` block line ranges. Clone
/// groups often cite the same file more than once (parser-fixture files
/// contain many parallel test functions) and tree-sitter parsing is the
/// dominant cost here, so cache the result keyed by relative path.
struct CfgTestBlockCache<'a> {
    project_root: &'a std::path::Path,
    inner: std::cell::RefCell<HashMap<String, Vec<(u32, u32)>>>,
}

impl<'a> CfgTestBlockCache<'a> {
    fn new(project_root: &'a std::path::Path) -> Self {
        Self {
            project_root,
            inner: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// Return cached `(start_line, end_line)` ranges for every
    /// `#[cfg(test)] mod ...` block in `rel_path`. Non-Rust files,
    /// unreadable files, and files with no inline test modules cache as
    /// an empty vector so each path parses at most once per call.
    fn ranges_for(&self, rel_path: &str, language: &str) -> Vec<(u32, u32)> {
        if language != "rust" {
            return Vec::new();
        }
        if let Some(cached) = self.inner.borrow().get(rel_path) {
            return cached.clone();
        }
        let abs = self.project_root.join(rel_path);
        let ranges = std::fs::read_to_string(&abs)
            .ok()
            .map(|src| crate::graph::security::find_cfg_test_blocks(&src))
            .unwrap_or_default();
        self.inner
            .borrow_mut()
            .insert(rel_path.to_string(), ranges.clone());
        ranges
    }
}

/// Drop every member of `group` whose file path looks like a test file or
/// whose line range sits inside a Rust `#[cfg(test)] mod tests {}` block.
/// Returns `None` when fewer than two distinct spans survive (a clone
/// group needs at least two).
fn filter_test_members(
    group: read::CloneGroup,
    cfg_test_cache: &CfgTestBlockCache<'_>,
) -> Option<read::CloneGroup> {
    let read::CloneGroup {
        shape_hash,
        symbols,
    } = group;
    let kept: Vec<_> = symbols
        .into_iter()
        .filter(|(sym, file)| {
            if helpers::is_test_path(&file.path) {
                return false;
            }
            let ranges = cfg_test_cache.ranges_for(&file.path, &file.language);
            !ranges
                .iter()
                .any(|(s, e)| sym.line_start >= *s && sym.line_end <= *e)
        })
        .collect();
    let distinct_spans: HashSet<(i64, u32, u32)> = kept
        .iter()
        .map(|(sym, _)| (sym.file_id, sym.line_start, sym.line_end))
        .collect();
    if distinct_spans.len() < 2 {
        return None;
    }
    Some(read::CloneGroup {
        shape_hash,
        symbols: kept,
    })
}
