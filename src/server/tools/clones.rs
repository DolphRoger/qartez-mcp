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
        description = "Detect duplicate code: groups of symbols with identical structural shape (same AST skeleton after normalizing identifiers, literals, and comments). Each group is a refactoring opportunity — extract the common logic into a shared function. Use min_lines to filter out trivial matches.",
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
        let min_lines = params.min_lines.unwrap_or(5);
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
