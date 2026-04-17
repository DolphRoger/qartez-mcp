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

#[tool_router(router = qartez_boundaries_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_boundaries",
        description = "Check architecture boundary rules defined in `.qartez/boundaries.toml` against the import graph. Each rule says files matching `from` must not import files matching any `deny` pattern (with optional `allow` overrides). Returns the list of violating edges. Pass `suggest=true` to emit a starter config derived from the current Leiden clustering instead of running the checker.",
        annotations(
            title = "Architecture Boundaries",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(in crate::server) fn qartez_boundaries(
        &self,
        Parameters(params): Parameters<SoulBoundariesParams>,
    ) -> Result<String, String> {
        use crate::graph::boundaries::{
            check_boundaries, load_config, render_config_toml, suggest_boundaries,
        };
        use crate::storage::read::{get_all_edges, get_all_file_clusters, get_all_files};

        let rel_path = params
            .config_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(".qartez/boundaries.toml");
        let abs_path = self.safe_resolve(rel_path)?;
        let concise = is_concise(&params.format);

        if params.suggest.unwrap_or(false) {
            let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
            let files = get_all_files(&conn).map_err(|e| format!("DB error: {e}"))?;
            let clusters = get_all_file_clusters(&conn).map_err(|e| format!("DB error: {e}"))?;
            let edges = get_all_edges(&conn).map_err(|e| format!("DB error: {e}"))?;
            drop(conn);

            if clusters.is_empty() {
                return Ok(
                    "No cluster assignment found. Run `qartez_wiki` first to compute \
                     clusters, then re-run `qartez_boundaries suggest=true`."
                        .to_string(),
                );
            }

            let cfg = suggest_boundaries(&files, &clusters, &edges);
            let toml = render_config_toml(&cfg);

            if let Some(write_rel) = params.write_to.as_deref().map(str::trim)
                && !write_rel.is_empty()
            {
                let write_abs = self.safe_resolve(write_rel)?;
                if let Some(parent) = write_abs.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
                }
                std::fs::write(&write_abs, &toml)
                    .map_err(|e| format!("Cannot write {}: {e}", write_abs.display()))?;
                return Ok(format!(
                    "Wrote {} rule(s) to {} ({} bytes).",
                    cfg.boundary.len(),
                    write_rel,
                    toml.len(),
                ));
            }

            if cfg.boundary.is_empty() {
                return Ok(
                    "No candidate rules: the current clustering has no directory-aligned \
                     partitions to derive rules from. Try `qartez_wiki recompute=true` first."
                        .to_string(),
                );
            }

            return Ok(toml);
        }

        if !abs_path.exists() {
            return Ok(format!(
                "No boundary config at {rel_path}. Run `qartez_boundaries suggest=true write_to={rel_path}` to generate a starter file."
            ));
        }
        let config = load_config(&abs_path).map_err(|e| format!("Config error: {e}"))?;

        let conn = self.db.lock().map_err(|e| format!("DB lock error: {e}"))?;
        let files = get_all_files(&conn).map_err(|e| format!("DB error: {e}"))?;
        let edges = get_all_edges(&conn).map_err(|e| format!("DB error: {e}"))?;
        drop(conn);

        let violations = check_boundaries(&config, &files, &edges);
        if violations.is_empty() {
            return Ok(format!(
                "No boundary violations across {} rule(s) and {} edges.",
                config.boundary.len(),
                edges.len(),
            ));
        }

        let mut out = String::new();
        out.push_str(&format!(
            "{} violation(s) across {} rule(s):\n",
            violations.len(),
            config.boundary.len(),
        ));

        if concise {
            for v in &violations {
                out.push_str(&format!(
                    "{} -> {} (rule #{}: deny {})\n",
                    v.from_file, v.to_file, v.rule_index, v.deny_pattern,
                ));
            }
            return Ok(out);
        }

        let mut current_rule: Option<usize> = None;
        for v in &violations {
            if current_rule != Some(v.rule_index) {
                current_rule = Some(v.rule_index);
                let rule = &config.boundary[v.rule_index];
                out.push_str(&format!(
                    "\nRule #{}: {} !-> {}\n",
                    v.rule_index,
                    rule.from,
                    rule.deny.join(" | "),
                ));
            }
            out.push_str(&format!(
                "  {} -> {} (matched deny pattern: {})\n",
                v.from_file, v.to_file, v.deny_pattern,
            ));
        }
        Ok(out)
    }
}
