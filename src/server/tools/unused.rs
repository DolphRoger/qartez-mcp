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

#[tool_router(router = qartez_unused_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_unused",
        description = "Find dead code: exported symbols with zero importers in the codebase. Safe candidates for removal or inlining. Pre-materialized at index time, so the whole-repo scan is a single indexed SELECT. Pass `limit` / `offset` to page through large result sets.",
        annotations(
            title = "Find Unused Exports",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(in crate::server) fn qartez_unused(
        &self,
        Parameters(params): Parameters<SoulUnusedParams>,
    ) -> Result<String, String> {
        let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;

        let limit = params.limit.unwrap_or(50).max(1) as i64;
        let offset = params.offset.unwrap_or(0) as i64;

        let total = read::count_unused_exports(&conn).map_err(|e| format!("DB error: {e}"))?;
        if total == 0 {
            return Ok("No unused exported symbols detected.".to_string());
        }

        let page = read::get_unused_exports_page(&conn, limit, offset)
            .map_err(|e| format!("DB error: {e}"))?;

        if page.is_empty() {
            return Ok(format!(
                "No unused exports in page (total={total}, offset={offset})."
            ));
        }

        let shown = page.len() as i64;
        let mut out = if shown < total {
            format!(
                "{total} unused export(s); showing {shown} from offset {offset} (next: offset={}).\n",
                offset + shown
            )
        } else {
            format!("{total} unused export(s).\n")
        };

        // Compact per-file format: one header per file, one line per symbol
        // without the parenthesized kind (it's redundant with the kind-letter
        // prefix). Saves ~40% tokens vs the old `  - name (kind) [L-L]` shape.
        let mut current_path: &str = "";
        for (sym, file) in &page {
            if file.path != current_path {
                out.push_str(&format!("{}\n", file.path));
                current_path = file.path.as_str();
            }
            out.push_str(&format!(
                "  {} {} L{}\n",
                sym.kind.chars().next().unwrap_or(' '),
                sym.name,
                sym.line_start,
            ));
        }
        Ok(out)
    }
}
