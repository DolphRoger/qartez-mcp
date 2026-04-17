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

#[tool_router(router = qartez_project_router, vis = "pub(super)")]
impl QartezServer {
    #[tool(
        name = "qartez_project",
        description = "Run project commands (test, build, lint, typecheck) with auto-detected toolchain (Cargo, npm/bun/yarn/pnpm, Go, Python, Dart/Flutter, Maven, Gradle, sbt, Ruby, Make). Use action='info' to see detected commands. Use filter for targeted runs (e.g., test name).",
        annotations(
            title = "Run Project Command",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(in crate::server) fn qartez_project(
        &self,
        Parameters(params): Parameters<SoulProjectParams>,
    ) -> Result<String, String> {
        let all_toolchains = toolchain::detect_all_toolchains(&self.project_root);
        let action = params.action.unwrap_or_default();

        if action == ProjectAction::Info {
            if all_toolchains.is_empty() {
                return Err("No recognized toolchain found. Looked for: Cargo.toml, package.json, go.mod, pyproject.toml, setup.py, pubspec.yaml, Gemfile, Makefile, pom.xml, build.gradle(.kts), build.sbt".to_string());
            }
            let mut out = String::new();
            for (i, tc) in all_toolchains.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                let available = toolchain::binary_available(&tc.build_tool);
                let marker = if available {
                    ""
                } else {
                    " (not found on PATH)"
                };
                out.push_str(&format!("# Project toolchain: {}{}\n\n", tc.name, marker,));
                out.push_str(&format!("Build tool: {}\n", tc.build_tool));
                out.push_str(&format!("Test:       {}\n", tc.test_cmd.join(" ")));
                out.push_str(&format!("Build:      {}\n", tc.build_cmd.join(" ")));
                if let Some(ref lint) = tc.lint_cmd {
                    out.push_str(&format!("Lint:       {}\n", lint.join(" ")));
                }
                if let Some(ref typecheck) = tc.typecheck_cmd {
                    out.push_str(&format!("Typecheck:  {}\n", typecheck.join(" ")));
                }
            }
            return Ok(out);
        }

        let tc = all_toolchains.into_iter().next().ok_or_else(|| {
            "No recognized toolchain found. Looked for: Cargo.toml, package.json, go.mod, pyproject.toml, setup.py, pubspec.yaml, Gemfile, Makefile, pom.xml, build.gradle(.kts), build.sbt".to_string()
        })?;

        if action == ProjectAction::Run {
            let subcommand = params.filter.as_deref().unwrap_or("test");
            let resolved: &Vec<String> = match subcommand {
                "test" => &tc.test_cmd,
                "build" => &tc.build_cmd,
                "lint" => tc.lint_cmd.as_ref().ok_or_else(|| {
                    format!("No lint command configured for {} toolchain", tc.name)
                })?,
                "typecheck" => tc.typecheck_cmd.as_ref().ok_or_else(|| {
                    format!("No typecheck command configured for {} toolchain", tc.name)
                })?,
                other => {
                    return Err(format!(
                        "Unknown run subcommand '{other}'. Supported: test, build, lint, typecheck",
                    ));
                }
            };
            return Ok(format!(
                "# {toolchain} {sub} (dry-run — command not executed)\n$ {cmd}\n",
                toolchain = tc.name,
                sub = subcommand,
                cmd = resolved.join(" "),
            ));
        }

        let (cmd, action_label): (&Vec<String>, &'static str) = match action {
            ProjectAction::Test => (&tc.test_cmd, "TEST"),
            ProjectAction::Build => (&tc.build_cmd, "BUILD"),
            ProjectAction::Lint => (
                tc.lint_cmd.as_ref().ok_or_else(|| {
                    format!("No lint command configured for {} toolchain", tc.name)
                })?,
                "LINT",
            ),
            ProjectAction::Typecheck => (
                tc.typecheck_cmd.as_ref().ok_or_else(|| {
                    format!("No typecheck command configured for {} toolchain", tc.name)
                })?,
                "TYPECHECK",
            ),
            ProjectAction::Info | ProjectAction::Run => {
                // Handled by the early-return branches above.
                unreachable!()
            }
        };

        let timeout = params.timeout.unwrap_or(60).min(600);
        let filter = params.filter.as_deref();
        if let Some(f) = filter
            && f.starts_with('-')
        {
            return Err(format!("Filter must not start with '-': {f}"));
        }

        let (exit_code, output) = toolchain::run_command(&self.project_root, cmd, filter, timeout)?;

        let status = if exit_code == 0 { "SUCCESS" } else { "FAILED" };
        let mut out = format!(
            "# {} {} (exit code: {})\n$ {}{}\n\n",
            action_label,
            status,
            exit_code,
            cmd.join(" "),
            filter.map(|f| format!(" {f}")).unwrap_or_default(),
        );
        out.push_str(&output);
        Ok(out)
    }
}
