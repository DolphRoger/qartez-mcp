//! Per-tool comparative benchmark harness for Qartez MCP.
//!
//! Measures each MCP tool against the equivalent non-MCP
//! workflow (`Glob`/`Grep`/`Read` as a Claude Code agent would run
//! them) and emits a per-tool matrix with token savings, latency, and
//! hand-authored verdicts.

pub mod grounding;
pub mod judge;
pub mod profiles;
pub mod report;
pub mod scenarios;
pub mod set_compare;
pub mod sim_runner;
pub mod targets;
pub mod tokenize;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::server::QartezServer;

pub use grounding::{FileFacts, GroundingScores};
pub use profiles::LanguageProfile;
pub use report::{BenchmarkReport, LatencyStats, ScenarioReport, SideReport, Verdict};
pub use scenarios::{SCENARIOS, Scenario, SimStep};
pub use targets::ResolvedTargets;

/// Configuration for the latency measurement loop.
///
/// Defaults are tuned for the scenario matrix on a live `.qartez`
/// database: 3 warmup runs to prime any lazy state, 7 measured runs, and
/// min/max trimming to absorb occasional cache-miss spikes. The resulting
/// 5 post-trim samples are enough to detect 10% efficiency regressions
/// without turning the bench into a long-running suite.
#[derive(Debug, Clone, Copy)]
pub struct LatencyConfig {
    pub warmup_runs: usize,
    pub measured_runs: usize,
    pub trim_outliers: bool,
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self {
            warmup_runs: 3,
            measured_runs: 7,
            trim_outliers: true,
        }
    }
}

/// Orchestrates per-scenario runs on both the MCP and simulated non-MCP sides.
pub struct BenchmarkRunner<'a> {
    pub server: &'a QartezServer,
    pub project_root: &'a Path,
    pub config: LatencyConfig,
    /// Optional map of scenario_id → cached non-MCP side from a prior run.
    /// When a scenario is found in this map its non-MCP side is taken verbatim
    /// from the cache, so iterative MCP-side development doesn't pay for the
    /// (much slower) Glob/Grep/Read/git-log simulation every time.
    pub non_mcp_cache: HashMap<String, SideReport>,
    /// Enable programmatic grounding verification for both MCP and non-MCP
    /// outputs. Set via [`with_grounding_enabled`](Self::with_grounding_enabled).
    /// Default is `false` so slice A runs (and the legacy `--judge` path)
    /// do not incur the grounding cost.
    pub grounding_enabled: bool,
    /// File verification cache shared across scenarios in a single
    /// `run_all` invocation. Wrapped in a `RefCell` so the grounding
    /// verifier can mutate it from inside `run_one(&self, ...)` without
    /// changing the existing `&self` signature (which slice A's
    /// `run_judge` relies on).
    pub file_cache: RefCell<HashMap<String, Option<FileFacts>>>,
    /// Symbol-lookup cache shared across scenarios, same pattern as
    /// [`file_cache`](Self::file_cache).
    pub symbol_cache: RefCell<HashMap<String, bool>>,
    /// Lazily-built basename index for resolving bare filenames
    /// (`config.json` with no directory). Populated on first miss.
    pub basename_index: RefCell<Option<HashMap<String, Vec<PathBuf>>>>,
}

impl<'a> BenchmarkRunner<'a> {
    pub fn new(server: &'a QartezServer, project_root: &'a Path) -> Self {
        Self {
            server,
            project_root,
            config: LatencyConfig::default(),
            non_mcp_cache: HashMap::new(),
            grounding_enabled: false,
            file_cache: RefCell::new(HashMap::new()),
            symbol_cache: RefCell::new(HashMap::new()),
            basename_index: RefCell::new(None),
        }
    }

    pub fn with_config(mut self, config: LatencyConfig) -> Self {
        self.config = config;
        self
    }

    /// Install a non-MCP cache built from a previously-serialized
    /// `BenchmarkReport`. Callers should verify the git SHA / codebase
    /// identity before loading the cache.
    pub fn with_non_mcp_cache(mut self, cache: HashMap<String, SideReport>) -> Self {
        self.non_mcp_cache = cache;
        self
    }

    /// Enable or disable programmatic grounding verification. Slice B
    /// builder hook; `--judge` sets this via
    /// `src/bin/benchmark.rs`.
    pub fn with_grounding_enabled(mut self, enabled: bool) -> Self {
        self.grounding_enabled = enabled;
        self
    }

    /// Compute claim-level grounding for `output`, reusing the runner's
    /// per-run caches. Returns `None` when grounding is disabled or the
    /// parser extracted zero claims. This is the single site that
    /// bridges `grounding::verify_side` and the runner's borrow state;
    /// both `run_mcp` and `run_sim` call it from inside the hot loop.
    fn grounding_for(&self, output: &str) -> Option<GroundingScores> {
        if !self.grounding_enabled {
            return None;
        }
        let conn_guard = self.server.db_connection();
        let mut file_cache = self.file_cache.borrow_mut();
        let mut symbol_cache = self.symbol_cache.borrow_mut();
        let mut basename_index = self.basename_index.borrow_mut();
        let mut ctx = grounding::GroundingContext {
            project_root: self.project_root,
            conn: Some(&*conn_guard),
            file_cache: &mut file_cache,
            symbol_cache: &mut symbol_cache,
            basename_index: &mut basename_index,
        };
        grounding::verify_side(output, &mut ctx)
    }

    /// Run every scenario, optionally filtered by substring match on
    /// tool name or id. Takes the active [`ResolvedTargets`] and
    /// [`LanguageProfile`] so the scenarios can be parameterized per
    /// language.
    pub fn run_all(
        &self,
        targets: &ResolvedTargets,
        profile: &LanguageProfile,
        filter: Option<&str>,
    ) -> Vec<ScenarioReport> {
        self.run_all_with_tier(targets, profile, filter, 1)
    }

    /// Like [`run_all`](Self::run_all) but accepts a maximum tier level.
    /// Scenarios with `tier > max_tier` are skipped.
    pub fn run_all_with_tier(
        &self,
        targets: &ResolvedTargets,
        profile: &LanguageProfile,
        filter: Option<&str>,
        max_tier: u8,
    ) -> Vec<ScenarioReport> {
        let mut reports = Vec::with_capacity(SCENARIOS.len());
        for scenario in SCENARIOS {
            if scenario.tier > max_tier {
                continue;
            }
            if let Some(f) = filter
                && !scenario.tool.contains(f)
                && !scenario.id.contains(f)
            {
                continue;
            }
            reports.push(self.run_one(scenario, targets, profile));
        }
        reports
    }

    pub fn run_one(
        &self,
        scenario: &Scenario,
        targets: &ResolvedTargets,
        profile: &LanguageProfile,
    ) -> ScenarioReport {
        let mcp = self.run_mcp(scenario, targets, profile);
        let sim = match self.non_mcp_cache.get(scenario.id) {
            Some(cached) => {
                let mut reused = cached.clone();
                reused.reused = true;
                reused
            }
            None => self.run_sim(scenario, targets, profile),
        };
        let set_comparison =
            set_compare::compare(scenario.tool, &mcp.full_output, &sim.full_output);
        let mut report = report::build_scenario_report(scenario, mcp, sim);
        report.set_comparison = set_comparison;
        report::fill_effective_savings(&mut report);
        report
    }

    fn run_mcp(
        &self,
        scenario: &Scenario,
        targets: &ResolvedTargets,
        profile: &LanguageProfile,
    ) -> SideReport {
        let args = (scenario.mcp_args)(targets, profile);
        let mut final_output = String::new();
        let mut final_error: Option<String> = None;

        for _ in 0..self.config.warmup_runs {
            let _ = self.server.call_tool_by_name(scenario.tool, args.clone());
        }

        let mut samples = Vec::with_capacity(self.config.measured_runs);
        for _ in 0..self.config.measured_runs {
            let start = Instant::now();
            match self.server.call_tool_by_name(scenario.tool, args.clone()) {
                Ok(out) => {
                    samples.push(start.elapsed());
                    final_output = out;
                    final_error = None;
                }
                Err(e) => {
                    samples.push(start.elapsed());
                    final_error = Some(e);
                }
            }
        }

        let latency = latency_from_samples(&samples, self.config.trim_outliers);
        let preview = preview_of(&final_output);
        let grounding = self.grounding_for(&final_output);
        SideReport {
            response_bytes: final_output.len(),
            response_preview: preview,
            tokens: tokenize::count_tokens(&final_output),
            naive_tokens: tokenize::naive_count(&final_output),
            latency,
            error: final_error,
            args: Some(args),
            steps: None,
            reused: false,
            full_output: final_output,
            grounding,
        }
    }

    fn run_sim(
        &self,
        scenario: &Scenario,
        targets: &ResolvedTargets,
        profile: &LanguageProfile,
    ) -> SideReport {
        let steps = (scenario.non_mcp_steps)(targets, profile);
        let sim_opts = sim_runner::Options {
            exclude_globs: profile.exclude_globs,
        };
        let mut final_output = String::new();
        let mut final_error: Option<String> = None;

        for _ in 0..self.config.warmup_runs {
            let _ = sim_runner::run_with(self.project_root, &steps, &sim_opts);
        }

        let mut samples = Vec::with_capacity(self.config.measured_runs);
        for _ in 0..self.config.measured_runs {
            let start = Instant::now();
            match sim_runner::run_with(self.project_root, &steps, &sim_opts) {
                Ok(out) => {
                    samples.push(start.elapsed());
                    final_output = out;
                    final_error = None;
                }
                Err(e) => {
                    samples.push(start.elapsed());
                    final_error = Some(e.to_string());
                }
            }
        }

        let latency = latency_from_samples(&samples, self.config.trim_outliers);
        let preview = preview_of(&final_output);
        let grounding = self.grounding_for(&final_output);
        SideReport {
            response_bytes: final_output.len(),
            response_preview: preview,
            tokens: tokenize::count_tokens(&final_output),
            naive_tokens: tokenize::naive_count(&final_output),
            latency,
            error: final_error,
            args: None,
            steps: Some(steps.iter().map(SimStep::describe).collect()),
            reused: false,
            full_output: final_output,
            grounding,
        }
    }
}

/// Build a non-MCP cache map from a previously-written `BenchmarkReport`.
///
/// When `expected_sha` is `Some`, the cache is only populated if the stored
/// report's `git_sha` matches - otherwise an empty map is returned, forcing
/// a fresh non-MCP run to avoid comparing against stale data.
pub fn build_non_mcp_cache(
    prior: &BenchmarkReport,
    expected_sha: Option<&str>,
) -> HashMap<String, SideReport> {
    // Invalidate whenever the caller pins a SHA and it does not match the
    // stored one. A prior report without a recorded SHA (older schema) is
    // treated as a miss when the caller wants SHA pinning - otherwise the
    // non-MCP numbers can be silently reused across unrelated revisions.
    if let Some(want) = expected_sha {
        match prior.git_sha.as_deref() {
            Some(have) if have == want => {}
            _ => return HashMap::new(),
        }
    }
    prior
        .scenarios
        .iter()
        .map(|s| (s.scenario_id.clone(), s.non_mcp.clone()))
        .collect()
}

/// Maximum preview length before the rest of the response is truncated.
/// Kept small to keep the JSON report under ~200 KB for 17 scenarios.
const PREVIEW_CAP_BYTES: usize = 240;

fn preview_of(s: &str) -> String {
    if s.len() <= PREVIEW_CAP_BYTES {
        s.to_string()
    } else {
        let mut cap = PREVIEW_CAP_BYTES;
        while !s.is_char_boundary(cap) && cap > 0 {
            cap -= 1;
        }
        format!("{}…", &s[..cap])
    }
}

fn latency_from_samples(samples: &[Duration], trim: bool) -> LatencyStats {
    let mut micros: Vec<f64> = samples.iter().map(|d| d.as_micros() as f64).collect();
    micros.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let effective: &[f64] = if trim && micros.len() >= 3 {
        &micros[1..micros.len() - 1]
    } else {
        &micros[..]
    };

    let n = effective.len() as f64;
    let mean = if n == 0.0 {
        0.0
    } else {
        effective.iter().sum::<f64>() / n
    };
    let variance = if n == 0.0 {
        0.0
    } else {
        effective.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n
    };
    let stdev = variance.sqrt();
    let p50 = percentile(effective, 50.0);
    let p95 = percentile(effective, 95.0);

    LatencyStats {
        mean_us: mean,
        stdev_us: stdev,
        p50_us: p50,
        p95_us: p95,
        samples: effective.len(),
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0) * (sorted.len() as f64 - 1.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        let frac = rank - lo as f64;
        sorted[lo] * (1.0 - frac) + sorted[hi] * frac
    }
}

/// Returns the short Git SHA of HEAD resolved from `project_root`, or
/// `None` if the `git` command is unavailable, the directory is not a
/// Git repository, or the output is empty.
///
/// The `project_root` argument pins the lookup to the repo the caller
/// actually cares about. Relying on the process CWD (as the previous
/// signature did) meant that invoking `qartez bench` from outside the
/// checkout - or from inside an unrelated repo - either returned `None`
/// or, worse, a foreign SHA that silently invalidated or reused the
/// benchmark cache against the wrong commit.
pub fn git_sha(project_root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .current_dir(project_root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `git_sha` must return `Option<String>` without panicking in any
    /// environment (no Git, detached HEAD, CI sandboxes). When it does return a
    /// short SHA, the SHA must be at least 4 hex characters - Git's minimum
    /// short-SHA length.
    #[test]
    fn git_sha_returns_some_or_none_without_panic() {
        let result: Option<String> = git_sha(Path::new("."));
        if let Some(sha) = result {
            assert!(
                sha.len() >= 4,
                "git short SHA should be at least 4 chars, got {sha:?}"
            );
        }
    }

    /// A non-existent root must not panic - `current_dir` resolution
    /// failure is surfaced as `None`, same as the "no git" branch.
    #[test]
    fn git_sha_missing_root_returns_none_without_panic() {
        let missing = Path::new("/nonexistent/qartez/benchmark/root/does/not/exist");
        assert!(git_sha(missing).is_none());
    }

    /// Build a minimal `BenchmarkReport` with one scenario via serde so the
    /// `build_non_mcp_cache` tests below do not have to hard-code 30+
    /// struct fields. Keeps the tests resilient to schema extensions.
    fn synthetic_report(git_sha: Option<&str>) -> BenchmarkReport {
        let sha_json = match git_sha {
            Some(s) => format!("\"{s}\""),
            None => "null".to_string(),
        };
        let side = r#"{
            "response_bytes": 0,
            "response_preview": "",
            "tokens": 0,
            "naive_tokens": 0,
            "latency": {"mean_us": 0.0, "stdev_us": 0.0, "p50_us": 0.0, "p95_us": 0.0, "samples": 0},
            "error": null
        }"#;
        let scenario = format!(
            r#"{{
                "tool": "qartez_find",
                "scenario_id": "scenario-1",
                "description": "",
                "mcp": {side},
                "non_mcp": {side},
                "savings": {{"tokens_pct": 0.0, "bytes_pct": 0.0, "latency_ratio": 1.0}},
                "verdict": {{"winner": "mcp", "pros": [], "cons": [], "summary": ""}}
            }}"#
        );
        let json = format!(
            r#"{{
                "generated_at_unix": 0,
                "git_sha": {sha_json},
                "tokenizer": "naive",
                "language": "rust",
                "scenarios": [{scenario}]
            }}"#
        );
        serde_json::from_str(&json).expect("synthetic report must parse")
    }

    #[test]
    fn build_non_mcp_cache_matches_when_sha_equal() {
        let prior = synthetic_report(Some("abc123"));
        let cache = build_non_mcp_cache(&prior, Some("abc123"));
        assert_eq!(cache.len(), 1, "SHA match must populate cache");
    }

    #[test]
    fn build_non_mcp_cache_empty_when_sha_differs() {
        let prior = synthetic_report(Some("abc123"));
        let cache = build_non_mcp_cache(&prior, Some("def456"));
        assert!(cache.is_empty(), "SHA mismatch must drop cache");
    }

    #[test]
    fn build_non_mcp_cache_empty_when_prior_sha_missing_but_expected_some() {
        // The exact regression: the old `(Some, Some)` pattern silently
        // accepted a stale `None`-SHA report, so downstream benchmarks
        // compared current MCP numbers against unrelated non-MCP numbers.
        let prior = synthetic_report(None);
        let cache = build_non_mcp_cache(&prior, Some("abc123"));
        assert!(
            cache.is_empty(),
            "prior report with no SHA must be rejected when caller pins one"
        );
    }

    #[test]
    fn build_non_mcp_cache_accepts_prior_when_caller_is_agnostic() {
        let prior = synthetic_report(Some("abc123"));
        let cache = build_non_mcp_cache(&prior, None);
        assert_eq!(
            cache.len(),
            1,
            "None expected_sha must accept any prior report"
        );
    }

    #[test]
    fn build_non_mcp_cache_accepts_when_both_none() {
        let prior = synthetic_report(None);
        let cache = build_non_mcp_cache(&prior, None);
        assert_eq!(cache.len(), 1);
    }
}
