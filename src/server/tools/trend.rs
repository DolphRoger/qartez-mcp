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

#[tool_router(router = qartez_trend_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_trend",
        description = "Show how a symbol's cyclomatic complexity changed over recent commits. Unlike qartez_hotspots (point-in-time), this reveals whether code is actively getting more complex (e.g. 'function grew from CC 8 to CC 39 over 5 commits'). Pass a file_path and optionally a symbol_name to focus on one function.",
        annotations(
            title = "Complexity Trend",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(in crate::server) fn qartez_trend(
        &self,
        Parameters(params): Parameters<SoulTrendParams>,
    ) -> Result<String, String> {
        if self.git_depth == 0 {
            return Err(
                "Complexity trend requires git history. Re-index with --git-depth > 0.".into(),
            );
        }

        let limit = params.limit.unwrap_or(10);
        let concise = matches!(params.format, Some(Format::Concise));

        let trends = crate::git::trend::complexity_trend(
            &self.project_root,
            &params.file_path,
            params.symbol_name.as_deref(),
            limit,
        )
        .map_err(|e| format!("trend analysis failed: {e}"))?;

        if trends.is_empty() {
            return Ok(format!(
                "No complexity trend data for `{}`. Possible reasons: file has fewer than 2 commits, no functions with measurable complexity, or symbol not found.",
                params.file_path
            ));
        }

        let mut out = String::new();

        if concise {
            out.push_str("# symbol commits first_cc last_cc delta% file\n");
            for t in &trends {
                let first_cc = t.points.first().map(|p| p.complexity).unwrap_or(0);
                let last_cc = t.points.last().map(|p| p.complexity).unwrap_or(0);
                let delta = if first_cc > 0 {
                    ((last_cc as f64 - first_cc as f64) / first_cc as f64 * 100.0) as i64
                } else {
                    0
                };
                out.push_str(&format!(
                    "{} {} {} {} {}% {}\n",
                    t.symbol_name,
                    t.points.len(),
                    first_cc,
                    last_cc,
                    delta,
                    t.file_path,
                ));
            }
        } else {
            out.push_str(&format!("# Complexity Trend: {}\n\n", params.file_path));

            for t in &trends {
                let first_cc = t.points.first().map(|p| p.complexity).unwrap_or(0);
                let last_cc = t.points.last().map(|p| p.complexity).unwrap_or(0);
                let delta = if first_cc > 0 {
                    (last_cc as f64 - first_cc as f64) / first_cc as f64 * 100.0
                } else {
                    0.0
                };

                let direction = if delta > 10.0 {
                    "GROWING"
                } else if delta < -10.0 {
                    "SHRINKING"
                } else {
                    "STABLE"
                };

                out.push_str(&format!(
                    "## {} ({}) CC {} -> {} ({:+.0}% {})\n\n",
                    t.symbol_name,
                    t.points.len(),
                    first_cc,
                    last_cc,
                    delta,
                    direction,
                ));

                out.push_str("  Commit  | CC | Lines | Summary\n");
                out.push_str("  --------+----+-------+--------\n");

                for (i, p) in t.points.iter().enumerate() {
                    let marker = if i > 0 {
                        let prev = t.points[i - 1].complexity;
                        if p.complexity > prev {
                            " (+)"
                        } else if p.complexity < prev {
                            " (-)"
                        } else {
                            ""
                        }
                    } else {
                        ""
                    };

                    out.push_str(&format!(
                        "  {} | {:>2}{:<4} | {:>5} | {}\n",
                        p.commit_sha, p.complexity, marker, p.line_count, p.commit_summary,
                    ));
                }
                out.push('\n');
            }
        }

        Ok(out)
    }
}
