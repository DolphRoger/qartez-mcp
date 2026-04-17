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

#[tool_router(router = qartez_cochange_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_cochange",
        description = "Find files that historically change together (from git history). High co-change count means files are logically coupled — modifying one likely requires modifying the other.",
        annotations(
            title = "Co-change History",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(in crate::server) fn qartez_cochange(
        &self,
        Parameters(params): Parameters<SoulCochangeParams>,
    ) -> Result<String, String> {
        let concise = is_concise(&params.format);
        let limit = params.limit.unwrap_or(10) as usize;
        let max_commit_size = params.max_commit_size.unwrap_or(30) as usize;

        {
            let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
            if read::get_file_by_path(&conn, &params.file_path)
                .map_err(|e| format!("DB error: {e}"))?
                .is_none()
            {
                return Err(format!("File '{}' not found in index", params.file_path));
            }
        }

        let pairs = compute_cochange_pairs(
            &self.project_root,
            &params.file_path,
            max_commit_size,
            self.git_depth as usize,
            limit,
        );

        let pairs = match pairs {
            Some(p) if !p.is_empty() => p,
            _ => {
                // Fallback: pre-computed table from index time. Useful when git
                // is unavailable or has been modified since indexing.
                let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
                let file = read::get_file_by_path(&conn, &params.file_path)
                    .map_err(|e| format!("DB error: {e}"))?
                    .ok_or_else(|| format!("File '{}' not found in index", params.file_path))?;
                let cc = read::get_cochanges(&conn, file.id, limit as i64)
                    .map_err(|e| format!("DB error: {e}"))?;
                if cc.is_empty() {
                    return Ok(format!(
                        "No co-change data found for '{}'. Run with git history available.",
                        params.file_path,
                    ));
                }
                cc.into_iter()
                    .map(|(c, f)| (f.path, c.count as u32))
                    .collect()
            }
        };

        if concise {
            let rendered: Vec<String> = pairs.iter().map(|(p, c)| format!("{p} ({c})")).collect();
            return Ok(format!(
                "Co-changes for {} (max_commit_size={}): {}",
                params.file_path,
                max_commit_size,
                rendered.join(", ")
            ));
        }

        let mut out = format!(
            "# Co-changes for: {} (max_commit_size={})\n\n",
            params.file_path, max_commit_size,
        );
        out.push_str(" # | File                                | Count\n");
        out.push_str("---+-------------------------------------+------\n");
        for (i, (path, count)) in pairs.iter().enumerate() {
            out.push_str(&format!(
                "{:>2} | {:<35} | {}\n",
                i + 1,
                truncate_path(path, 35),
                count,
            ));
        }
        Ok(out)
    }
}
