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

#[tool_router(router = qartez_calls_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_calls",
        description = "Show call hierarchy for a function: who calls it (callers) and what it calls (callees). Uses tree-sitter AST analysis. Distinguishes actual calls from type annotations, unlike qartez_refs.",
        annotations(
            title = "Call Hierarchy",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(in crate::server) fn qartez_calls(
        &self,
        Parameters(params): Parameters<SoulCallsParams>,
    ) -> Result<String, String> {
        let concise = is_concise(&params.format);
        let direction = params.direction.unwrap_or_default();
        let want_callers = matches!(direction, CallDirection::Callers | CallDirection::Both);
        let want_callees = matches!(direction, CallDirection::Callees | CallDirection::Both);
        // Depth=1 is the default after the 2026-04 compaction: depth=2 can
        // explode on hub functions, so callers opt in explicitly.
        let max_depth = params.depth.unwrap_or(1) as usize;

        // Lock 1: resolve the target symbol and fetch the file list.
        let (symbols, all_files) = {
            let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
            let symbols = read::find_symbol_by_name(&conn, &params.name)
                .map_err(|e| format!("DB error: {e}"))?;
            let all_files = if want_callers {
                read::get_all_files(&conn).map_err(|e| format!("DB error: {e}"))?
            } else {
                Vec::new()
            };
            (symbols, all_files)
        };

        if symbols.is_empty() {
            return Err(format!("No symbol '{}' found in index", params.name));
        }

        let func_symbols: Vec<_> = symbols
            .iter()
            .filter(|(s, _)| matches!(s.kind.as_str(), "function" | "method" | "constructor"))
            .collect();

        if func_symbols.is_empty() {
            return Err(format!(
                "'{}' exists but is not a function/method",
                params.name
            ));
        }

        if is_mermaid(&params.format) {
            return self.qartez_calls_mermaid(
                &params.name,
                &func_symbols,
                &all_files,
                want_callers,
                want_callees,
            );
        }

        let mut out = String::new();
        // Per-invocation caches. Both sets overlap heavily inside a single
        // tool call, so memoizing avoids re-running SQL.
        let mut resolve_cache: HashMap<
            String,
            Vec<(
                crate::storage::models::SymbolRow,
                crate::storage::models::FileRow,
            )>,
        > = HashMap::new();
        let mut file_syms_cache: HashMap<i64, Vec<crate::storage::models::SymbolRow>> =
            HashMap::new();

        for (sym, def_file) in &func_symbols {
            out.push_str(&format!(
                "{} ({}) @ {}:L{}-{}\n",
                sym.name, sym.kind, def_file.path, sym.line_start, sym.line_end,
            ));

            if want_callers {
                self.append_callers(
                    &params.name,
                    &all_files,
                    &mut file_syms_cache,
                    &mut out,
                    concise,
                )?;
            }

            if want_callees {
                self.append_callees(sym, def_file, &mut resolve_cache, &mut out, concise)?;
            }

            if max_depth > 1 && want_callees {
                self.append_depth2(sym, def_file, &mut resolve_cache, &mut out)?;
            }
        }

        Ok(out)
    }
}

impl QartezServer {
    fn append_callers(
        &self,
        name: &str,
        all_files: &[crate::storage::models::FileRow],
        file_syms_cache: &mut HashMap<i64, Vec<crate::storage::models::SymbolRow>>,
        out: &mut String,
        concise: bool,
    ) -> Result<(), String> {
        // Scan phase (no lock): FS reads + tree-sitter parsing for every
        // file. This is the heaviest phase and must not hold the db mutex.
        let mut raw_sites: Vec<(i64, String, Vec<usize>)> = Vec::new();
        for file in all_files {
            let source = match self.cached_source(&file.path) {
                Some(s) => s,
                None => continue,
            };
            if !source.contains(name) {
                continue;
            }
            let calls = self.cached_calls(&file.path);
            let matching: Vec<usize> = calls
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, l)| *l)
                .collect();
            if !matching.is_empty() {
                raw_sites.push((file.id, file.path.clone(), matching));
            }
        }

        // Resolve phase (lock 2): fetch per-file symbol lists to find the
        // enclosing function for each call site.
        let mut sites: Vec<(String, usize, Option<String>)> = Vec::new();
        if !raw_sites.is_empty() {
            let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
            for (file_id, file_path, matching) in &raw_sites {
                let file_syms = file_syms_cache.entry(*file_id).or_insert_with(|| {
                    read::get_symbols_for_file(&conn, *file_id).unwrap_or_default()
                });
                for line in matching {
                    let enclosing = file_syms
                        .iter()
                        .filter(|s| {
                            s.line_start as usize <= *line
                                && *line <= s.line_end as usize
                                && matches!(s.kind.as_str(), "function" | "method" | "constructor")
                        })
                        .max_by_key(|s| s.line_start)
                        .map(|s| s.name.clone());
                    sites.push((file_path.clone(), *line, enclosing));
                }
            }
        }

        if sites.is_empty() {
            out.push_str("callers: none\n");
        } else {
            out.push_str(&format!("callers: {}\n", sites.len()));
            if !concise {
                for (path, line, encl) in &sites {
                    match encl {
                        Some(fn_name) => out.push_str(&format!("  {fn_name} @ {path}:L{line}\n")),
                        None => out.push_str(&format!("  (top) @ {path}:L{line}\n")),
                    }
                }
            }
        }
        Ok(())
    }

    fn append_callees(
        &self,
        sym: &crate::storage::models::SymbolRow,
        def_file: &crate::storage::models::FileRow,
        resolve_cache: &mut HashMap<
            String,
            Vec<(
                crate::storage::models::SymbolRow,
                crate::storage::models::FileRow,
            )>,
        >,
        out: &mut String,
        concise: bool,
    ) -> Result<(), String> {
        let all_calls = self.cached_calls(&def_file.path);
        let start = sym.line_start as usize;
        let end = sym.line_end as usize;
        // Dedup by name; keep the first-seen line only so long functions
        // don't blow up the output.
        let mut seen_order: Vec<String> = Vec::new();
        let mut first_line: HashMap<String, usize> = HashMap::new();
        for (name, line) in all_calls.iter() {
            if *line < start || *line > end {
                continue;
            }
            if !first_line.contains_key(name) {
                first_line.insert(name.clone(), *line);
                seen_order.push(name.clone());
            }
        }

        if seen_order.is_empty() {
            out.push_str("callees: none\n");
            return Ok(());
        }

        out.push_str(&format!("callees: {}\n", seen_order.len()));
        if concise {
            return Ok(());
        }

        {
            let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
            for callee_name in &seen_order {
                resolve_cache.entry(callee_name.clone()).or_insert_with(|| {
                    read::find_symbol_by_name(&conn, callee_name).unwrap_or_default()
                });
            }
        }
        for callee_name in &seen_order {
            let resolved = resolve_cache.get(callee_name).unwrap();
            match resolved.first() {
                Some((_, f)) => out.push_str(&format!("  {callee_name} @ {}\n", f.path)),
                None => out.push_str(&format!("  {callee_name} (extern)\n")),
            }
        }
        Ok(())
    }

    fn append_depth2(
        &self,
        sym: &crate::storage::models::SymbolRow,
        def_file: &crate::storage::models::FileRow,
        resolve_cache: &mut HashMap<
            String,
            Vec<(
                crate::storage::models::SymbolRow,
                crate::storage::models::FileRow,
            )>,
        >,
        out: &mut String,
    ) -> Result<(), String> {
        let all_calls = self.cached_calls(&def_file.path);
        let start = sym.line_start as usize;
        let end = sym.line_end as usize;
        let direct: Vec<String> = {
            let mut seen = HashSet::new();
            let mut ordered = Vec::new();
            for (n, l) in all_calls.iter() {
                if *l >= start && *l <= end && seen.insert(n.clone()) {
                    ordered.push(n.clone());
                }
            }
            ordered
        };

        // Global visited set protects against cycles and hub blow-up: the
        // root function and every direct callee are seeded so A → B → A
        // or self-recursion doesn't reappear at depth 2, and a target
        // reached from one root isn't re-listed under another.
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(sym.name.clone());
        for d in &direct {
            visited.insert(d.clone());
        }

        {
            let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
            for callee_name in &direct {
                resolve_cache.entry(callee_name.clone()).or_insert_with(|| {
                    read::find_symbol_by_name(&conn, callee_name).unwrap_or_default()
                });
            }
        }

        let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
        for callee_name in &direct {
            let resolved = resolve_cache.get(callee_name).unwrap();
            let mut targets: Vec<String> = Vec::new();
            for (s2, f2) in resolved.iter() {
                if !matches!(s2.kind.as_str(), "function" | "method") {
                    continue;
                }
                let calls2 = self.cached_calls(&f2.path);
                let s2start = s2.line_start as usize;
                let s2end = s2.line_end as usize;
                for (n, l) in calls2.iter() {
                    if *l >= s2start && *l <= s2end && !visited.contains(n) {
                        visited.insert(n.clone());
                        targets.push(n.clone());
                    }
                }
            }
            if !targets.is_empty() {
                grouped.push((callee_name.clone(), targets));
            }
        }
        if grouped.is_empty() {
            out.push_str("depth2: none\n");
        } else {
            out.push_str("depth2:\n");
            for (root, targets) in &grouped {
                if targets.len() == 1 {
                    out.push_str(&format!("  {} → {}\n", root, targets[0]));
                } else {
                    out.push_str(&format!("  {} → {{{}}}\n", root, targets.join(", ")));
                }
            }
        }
        Ok(())
    }
}

impl QartezServer {
    /// Render call hierarchy as a Mermaid flowchart.
    fn qartez_calls_mermaid(
        &self,
        target_name: &str,
        func_symbols: &[&(
            crate::storage::models::SymbolRow,
            crate::storage::models::FileRow,
        )],
        all_files: &[crate::storage::models::FileRow],
        want_callers: bool,
        want_callees: bool,
    ) -> Result<String, String> {
        let max_nodes = 50;
        let mut out = String::from("graph TD\n");
        let target_id = helpers::mermaid_node_id(target_name);
        let target_label = helpers::mermaid_label(target_name);
        out.push_str(&format!("  {target_id}[\"{target_label}\"]\n"));

        let mut count = 0usize;
        let mut seen_edges = HashSet::new();

        for (sym, def_file) in func_symbols {
            if want_callers {
                for file in all_files {
                    if count >= max_nodes {
                        break;
                    }
                    let source = match self.cached_source(&file.path) {
                        Some(s) => s,
                        None => continue,
                    };
                    if !source.contains(target_name) {
                        continue;
                    }
                    let calls = self.cached_calls(&file.path);
                    let has_call = calls.iter().any(|(name, _)| name == target_name);
                    if !has_call {
                        continue;
                    }
                    let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
                    let file_syms = read::get_symbols_for_file(&conn, file.id).unwrap_or_default();
                    drop(conn);
                    let matching_lines: Vec<usize> = calls
                        .iter()
                        .filter(|(name, _)| name == target_name)
                        .map(|(_, l)| *l)
                        .collect();
                    for line in &matching_lines {
                        if count >= max_nodes {
                            break;
                        }
                        let enclosing = file_syms
                            .iter()
                            .filter(|s| {
                                s.line_start as usize <= *line
                                    && *line <= s.line_end as usize
                                    && matches!(
                                        s.kind.as_str(),
                                        "function" | "method" | "constructor"
                                    )
                            })
                            .max_by_key(|s| s.line_start)
                            .map(|s| s.name.clone());
                        let caller = enclosing.as_deref().unwrap_or("(top-level)");
                        let cid = helpers::mermaid_node_id(caller);
                        let edge_key = format!("{cid}-->{target_id}");
                        if !seen_edges.insert(edge_key) {
                            continue;
                        }
                        let clabel = helpers::mermaid_label(caller);
                        out.push_str(&format!("  {cid}[\"{clabel}\"] --> {target_id}\n"));
                        count += 1;
                    }
                }
            }

            if want_callees {
                let all_calls = self.cached_calls(&def_file.path);
                let start = sym.line_start as usize;
                let end = sym.line_end as usize;
                let mut seen = HashSet::new();
                for (name, line) in all_calls.iter() {
                    if count >= max_nodes {
                        break;
                    }
                    if *line < start || *line > end {
                        continue;
                    }
                    if !seen.insert(name.clone()) {
                        continue;
                    }
                    let cid = helpers::mermaid_node_id(name);
                    let clabel = helpers::mermaid_label(name);
                    out.push_str(&format!("  {target_id} --> {cid}[\"{clabel}\"]\n"));
                    count += 1;
                }
            }
        }

        if count >= max_nodes {
            out.push_str("  truncated[\"... truncated\"]\n");
        }
        Ok(out)
    }
}
