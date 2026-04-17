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

#[tool_router(router = qartez_tools_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_tools",
        description = "Discover and enable additional Qartez tools. Call with no arguments to see all available tiers and tools. Use enable/disable to dynamically add or remove tool tiers or individual tools. Tier names: 'core' (always on), 'analysis', 'refactor', 'meta'. Pass 'all' to enable everything.",
        annotations(
            title = "Tool Discovery",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(in crate::server) async fn qartez_tools(
        &self,
        context: RequestContext<RoleServer>,
        Parameters(params): Parameters<ToolsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let is_listing = params.enable.is_none() && params.disable.is_none();

        if is_listing {
            let enabled = self
                .enabled_tools
                .read()
                .expect("enabled_tools lock poisoned");
            let mut out = String::from("# Qartez Tool Tiers\n\n");
            for &tier_name in tiers::ALL_TIER_NAMES {
                let tools = tiers::tier_tools(tier_name).unwrap_or_default();
                let all_enabled = tools.iter().all(|t| enabled.contains(*t));
                let status = if all_enabled { "enabled" } else { "disabled" };
                out.push_str(&format!("## {tier_name} ({status})\n"));
                for &tool_name in tools {
                    let mark = if enabled.contains(tool_name) {
                        "x"
                    } else {
                        " "
                    };
                    let desc = self
                        .tool_router
                        .get(tool_name)
                        .map(|t| t.description.as_deref().unwrap_or(""))
                        .unwrap_or("");
                    let short = desc.split('.').next().unwrap_or(desc);
                    out.push_str(&format!("- [{mark}] `{tool_name}` -- {short}\n"));
                }
                out.push('\n');
            }
            out.push_str("Use `enable: [\"analysis\"]` or `enable: [\"all\"]` to unlock tiers.\n");
            out.push_str("Use `disable: [\"refactor\"]` to hide tiers.\n");
            return Ok(CallToolResult::success(vec![Content::text(out)]));
        }

        let mut changed = false;
        {
            let mut enabled = self
                .enabled_tools
                .write()
                .expect("enabled_tools lock poisoned");

            if let Some(ref targets) = params.enable {
                for target in targets {
                    if target == "all" {
                        let all_tools = self.tool_router.list_all();
                        for tool in &all_tools {
                            if enabled.insert(tool.name.to_string()) {
                                changed = true;
                            }
                        }
                    } else if let Some(tools) = tiers::tier_tools(target) {
                        for &name in tools {
                            if enabled.insert(name.to_owned()) {
                                changed = true;
                            }
                        }
                    } else if self.tool_router.get(target).is_some()
                        && enabled.insert(target.clone())
                    {
                        changed = true;
                    }
                }
            }

            if let Some(ref targets) = params.disable {
                for target in targets {
                    if target == "core" || target == tiers::META_TOOL_NAME {
                        continue;
                    }
                    if let Some(tools) = tiers::tier_tools(target) {
                        for &name in tools {
                            if enabled.remove(name) {
                                changed = true;
                            }
                        }
                    } else if target != tiers::META_TOOL_NAME && enabled.remove(target.as_str()) {
                        changed = true;
                    }
                }
            }
        }

        if changed {
            let _ = context.peer.notify_tool_list_changed().await;
        }

        let enabled = self
            .enabled_tools
            .read()
            .expect("enabled_tools lock poisoned");
        let count = enabled.len();
        let msg = if changed {
            format!("Tool list updated. {count} tools now enabled.")
        } else {
            format!("No changes. {count} tools currently enabled.")
        };
        Ok(CallToolResult::success(vec![Content::text(msg)]))
    }
}
