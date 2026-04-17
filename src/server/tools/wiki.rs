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

#[tool_router(router = qartez_wiki_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_wiki",
        description = "Generate a markdown architecture wiki from Leiden-style community detection on the import graph. Partitions files into clusters, names each by the shared path prefix or top-PageRank file, and emits ARCHITECTURE.md with per-cluster file lists, top exported symbols, and inter-cluster edges. Use write_to=null to return the markdown as a string, or write_to=<path> to save to disk. Resolution controls cluster granularity (default 1.0; higher = more clusters).",
        annotations(
            title = "Architecture Wiki",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(in crate::server) fn qartez_wiki(
        &self,
        Parameters(params): Parameters<SoulWikiParams>,
    ) -> Result<String, String> {
        use crate::graph::boundaries::{check_boundaries, load_config};
        use crate::graph::leiden::LeidenConfig;
        use crate::graph::wiki::{WikiConfig, render_wiki};
        use crate::storage::read::{get_all_edges, get_all_files};

        let leiden = LeidenConfig {
            resolution: params.resolution.unwrap_or(1.0),
            min_cluster_size: params.min_cluster_size.unwrap_or(3),
            ..Default::default()
        };
        let mut wiki_cfg = WikiConfig {
            project_name: self.project_name(),
            max_files_per_section: params
                .max_files_per_section
                .map(|v| v as usize)
                .unwrap_or(20),
            recompute: params.recompute.unwrap_or(false),
            leiden,
            ..Default::default()
        };

        let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;

        let boundary_config_path = self.project_root.join(".qartez/boundaries.toml");
        if boundary_config_path.exists() {
            match load_config(&boundary_config_path) {
                Ok(cfg) => {
                    let files = get_all_files(&conn).map_err(|e| format!("DB error: {e}"))?;
                    let edges = get_all_edges(&conn).map_err(|e| format!("DB error: {e}"))?;
                    wiki_cfg.boundary_violations = Some(check_boundaries(&cfg, &files, &edges));
                }
                Err(e) => {
                    tracing::warn!("boundary config parse failed: {e}");
                }
            }
        }

        let (markdown, modularity) =
            render_wiki(&conn, &wiki_cfg).map_err(|e| format!("Wiki render error: {e}"))?;
        drop(conn);

        let mod_line = modularity
            .map(|q| format!(", modularity {q:.2}"))
            .unwrap_or_default();
        let cluster_count = markdown
            .lines()
            .filter(|l| l.starts_with("## ") && !l.starts_with("## Table of contents"))
            .count();

        if let Some(path) = params.write_to.as_deref().map(str::trim)
            && !path.is_empty()
        {
            let abs = self.safe_resolve(path)?;
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
            }
            std::fs::write(&abs, &markdown)
                .map_err(|e| format!("Cannot write {}: {e}", abs.display()))?;
            return Ok(format!(
                "Wrote {} bytes to {} ({} clusters{})",
                markdown.len(),
                path,
                cluster_count,
                mod_line,
            ));
        }

        Ok(markdown)
    }
}
