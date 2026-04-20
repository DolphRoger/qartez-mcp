use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::cli::Cli;
use crate::error::Result;

pub struct Config {
    pub project_roots: Vec<PathBuf>,
    pub root_aliases: HashMap<PathBuf, String>,
    pub primary_root: PathBuf,
    pub db_path: PathBuf,
    pub reindex: bool,
    pub git_depth: u32,
    pub has_project: bool,
}

const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
];

fn detect_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        for marker in PROJECT_MARKERS {
            if current.join(marker).exists() {
                return Some(current);
            }
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Scan immediate children of `dir` for project markers (e.g. `.git`).
/// Handles the meta-directory pattern where a folder groups multiple repos.
fn detect_child_project_roots(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut roots: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let has_marker = PROJECT_MARKERS.iter().any(|m| path.join(m).exists());
            has_marker.then_some(path)
        })
        .collect();

    roots.sort();
    roots
}

fn detect_qartez_workspace(root: &Path) -> (Vec<PathBuf>, HashMap<PathBuf, String>) {
    let config_path = root.join(".qartez").join("workspace.toml");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (Vec::new(), HashMap::new());
        }
        Err(e) => {
            tracing::warn!("failed to read {}: {e}", config_path.display());
            return (Vec::new(), HashMap::new());
        }
    };

    let doc: toml_edit::DocumentMut = match content.parse() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "failed to parse {}: {e} - workspace disabled",
                config_path.display()
            );
            return (Vec::new(), HashMap::new());
        }
    };

    let mut roots = Vec::new();
    let mut aliases = HashMap::new();

    if let Some(workspaces) = doc.get("workspaces").and_then(|w| w.as_table()) {
        for (alias, value) in workspaces.iter() {
            let path_str = match value.as_str() {
                Some(s) => s,
                None => continue,
            };

            let path = expand_path(path_str, root);

            if let Ok(canonical) = path.canonicalize() {
                roots.push(canonical.clone());
                aliases.insert(canonical, alias.to_string());
            } else if path.is_dir() {
                roots.push(path.clone());
                aliases.insert(path, alias.to_string());
            }
        }
    }

    (roots, aliases)
}

/// Detect workspace member directories from workspace config files.
///
/// Checks for npm/yarn/pnpm (`package.json` `"workspaces"`), Cargo
/// (`Cargo.toml` `[workspace] members`), and Go (`go.work` `use`
/// directives). Returned paths are absolute and sorted.
fn detect_workspace_members(root: &Path) -> (Vec<PathBuf>, HashMap<PathBuf, String>) {
    let mut members = Vec::new();
    let (qartez_roots, qartez_aliases) = detect_qartez_workspace(root);
    members.extend(qartez_roots);
    members.extend(detect_npm_workspace(root));
    members.extend(detect_cargo_workspace(root));
    members.extend(detect_go_workspace(root));
    members.sort();
    members.dedup();
    (members, qartez_aliases)
}

/// Parse `package.json` `"workspaces"` field and expand globs.
fn detect_npm_workspace(root: &Path) -> Vec<PathBuf> {
    let pkg_path = root.join("package.json");
    let content = match std::fs::read_to_string(&pkg_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // "workspaces" can be an array or an object with a "packages" key
    let patterns: Vec<&str> = match &json["workspaces"] {
        serde_json::Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
        serde_json::Value::Object(obj) => match obj.get("packages") {
            Some(serde_json::Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str()).collect(),
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };

    expand_workspace_globs(root, &patterns)
}

/// Parse `Cargo.toml` `[workspace] members` and expand globs.
fn detect_cargo_workspace(root: &Path) -> Vec<PathBuf> {
    let cargo_path = root.join("Cargo.toml");
    let content = match std::fs::read_to_string(&cargo_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let doc: toml_edit::DocumentMut = match content.parse() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let members = match doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    {
        Some(arr) => arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>(),
        None => return Vec::new(),
    };

    expand_workspace_globs(root, &members)
}

/// Parse `go.work` `use` directives.
fn detect_go_workspace(root: &Path) -> Vec<PathBuf> {
    let go_work = root.join("go.work");
    let content = match std::fs::read_to_string(&go_work) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut results = Vec::new();
    let mut in_use_block = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "use (" {
            in_use_block = true;
            continue;
        }
        if in_use_block && trimmed == ")" {
            in_use_block = false;
            continue;
        }

        let dir = if in_use_block {
            trimmed.trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace())
        } else if let Some(rest) = trimmed.strip_prefix("use ") {
            rest.trim().trim_matches(|c: char| c == '"' || c == '\'')
        } else {
            continue;
        };

        if dir.is_empty() || dir.starts_with("//") {
            continue;
        }
        let abs = root.join(dir);
        if abs.is_dir() {
            results.push(abs);
        }
    }

    results
}

/// Expand workspace glob patterns (e.g. `"packages/*"`) relative to a root
/// directory. Splits each pattern into a literal parent and a glob tail,
/// walks the parent directory, and matches entries with `globset`. Only
/// returns directories that actually exist on disk.
fn expand_workspace_globs(root: &Path, patterns: &[&str]) -> Vec<PathBuf> {
    let mut results = Vec::new();
    for pattern in patterns {
        let pat_path = Path::new(pattern);

        // If the pattern has no glob characters, treat it as a literal path
        if !pattern.contains('*') && !pattern.contains('?') && !pattern.contains('[') {
            let candidate = root.join(pattern);
            if candidate.is_dir() {
                results.push(candidate);
            }
            continue;
        }

        // Split into literal parent dir and glob filename component.
        // e.g. "packages/*" -> parent="packages", glob_part="*"
        let (parent_rel, glob_part) = match pat_path.parent() {
            Some(p) if !p.as_os_str().is_empty() => (
                p.to_string_lossy().to_string(),
                pat_path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
            _ => (String::new(), pattern.to_string()),
        };

        let scan_dir = if parent_rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&parent_rel)
        };

        let Ok(entries) = std::fs::read_dir(&scan_dir) else {
            continue;
        };

        let Ok(matcher) = globset::GlobBuilder::new(&glob_part)
            .literal_separator(true)
            .build()
            .map(|g| g.compile_matcher())
        else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            if matcher.is_match(name.to_string_lossy().as_ref()) {
                results.push(path);
            }
        }
    }
    results
}

/// For each root, check if it has a workspace config and expand its members
/// into additional roots. The original root is kept (it may contain shared
/// config, scripts, etc.), and members are appended after it. Duplicates
/// are removed.
fn expand_roots_with_workspaces(roots: Vec<PathBuf>) -> (Vec<PathBuf>, HashMap<PathBuf, String>) {
    let mut expanded = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut all_aliases = HashMap::new();
    for root in &roots {
        let canonical = normalize_for_dedup(root);
        if seen.insert(canonical) {
            expanded.push(root.clone());
        }
        let (members, aliases) = detect_workspace_members(root);
        all_aliases.extend(aliases);
        for member in members {
            let member_canonical = normalize_for_dedup(&member);
            if seen.insert(member_canonical) {
                expanded.push(member);
            }
        }
    }
    (expanded, all_aliases)
}

/// Produce a stable key for deduplicating roots. Prefer filesystem-canonical
/// form (resolves symlinks, absolute path), but fall back to an absolute +
/// lexically-normalized form when canonicalize fails (missing path, broken
/// symlink, insufficient permissions). The fallback collapses `.` and `..`
/// components without touching the filesystem, so two spellings of the same
/// missing directory (e.g. `foo` and `foo/../foo`) share a dedup key.
///
/// Pre-fix this function used `canonicalize().unwrap_or_else(|_| path.clone())`,
/// which kept the raw user input on failure and let different spellings of
/// the same root both escape dedup.
fn normalize_for_dedup(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Expand a path string relative to `base`, handling `~/` home expansion.
pub(crate) fn expand_path(path_str: &str, base: &Path) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/") {
        if let Some(home) = cross_platform_home() {
            return home.join(rest);
        }
    }
    base.join(path_str)
}

/// Cross-platform home directory detection.
/// Checks HOME (Unix), USERPROFILE (Windows), HOMEDRIVE+HOMEPATH (Windows).
pub fn cross_platform_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home));
    }
    // Try USERPROFILE (Windows)
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return Some(PathBuf::from(profile));
    }
    // Try HOMEDRIVE+HOMEPATH (Windows fallback)
    if let (Some(drive), Some(path)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let mut combined = PathBuf::from(drive);
        combined.push(path);
        if combined.is_dir() {
            return Some(combined);
        }
    }
    None
}

fn is_home_dir(path: &Path) -> bool {
    cross_platform_home().is_some_and(|home| path == home)
}

impl Config {
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let (project_roots, has_project) = if cli.root.is_empty() {
            let cwd_is_project =
                !is_home_dir(&cwd) && PROJECT_MARKERS.iter().any(|m| cwd.join(m).exists());
            if cwd_is_project {
                (vec![cwd.clone()], true)
            } else {
                // No markers in cwd - check for child project roots (meta-directory)
                let children = detect_child_project_roots(&cwd);
                if !children.is_empty() {
                    (children, true)
                } else {
                    // Walk up, but reject home directory to avoid indexing ~
                    match detect_project_root(&cwd) {
                        Some(root) if !is_home_dir(&root) => (vec![root], true),
                        _ => (vec![cwd.clone()], false),
                    }
                }
            }
        } else {
            (cli.root.clone(), true)
        };

        // Expand workspace members: if any root has a workspace config
        // (npm, Cargo, Go), add member directories as additional roots.
        let (project_roots, root_aliases) = expand_roots_with_workspaces(project_roots);

        let primary_root = project_roots[0].clone();

        // For multi-root (meta-directory), store the database in cwd
        let db_anchor = if project_roots.len() > 1 {
            &cwd
        } else {
            &primary_root
        };
        let db_path = match &cli.db_path {
            Some(p) => p.clone(),
            None => db_anchor.join(".qartez").join("index.db"),
        };

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        Ok(Config {
            project_roots,
            root_aliases,
            primary_root,
            db_path,
            reindex: cli.reindex,
            git_depth: cli.git_depth,
            has_project,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_detect_qartez_workspace_expansion() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let other = TempDir::new().unwrap();
        let other_path = other.path();

        let qartez_dir = root.join(".qartez");
        std::fs::create_dir_all(&qartez_dir).unwrap();

        let config_toml = format!(
            r#"
[workspaces]
Other = "{}"
Relative = "subproject"
"#,
            other_path.to_string_lossy().replace('\\', "/")
        );
        std::fs::write(qartez_dir.join("workspace.toml"), config_toml).unwrap();

        let subproject = root.join("subproject");
        std::fs::create_dir_all(&subproject).unwrap();

        let (roots, aliases) = detect_qartez_workspace(root);

        assert_eq!(roots.len(), 2);

        let other_canonical = other_path
            .canonicalize()
            .unwrap_or(other_path.to_path_buf());
        let sub_canonical = subproject.canonicalize().unwrap_or(subproject);

        assert!(roots.contains(&other_canonical));
        assert!(roots.contains(&sub_canonical));

        assert_eq!(aliases.get(&other_canonical).unwrap(), "Other");
        assert_eq!(aliases.get(&sub_canonical).unwrap(), "Relative");
    }

    /// Two PathBuf values that point at the same real directory must produce
    /// the same dedup key even when one form is canonical and the other isn't.
    #[test]
    fn normalize_for_dedup_matches_canonical_form() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("project");
        std::fs::create_dir_all(&real).unwrap();

        // Construct a non-canonical (has `..`) view of the same directory.
        let dotted = tmp.path().join("project").join("..").join("project");
        assert!(dotted.exists());

        let key_real = normalize_for_dedup(&real);
        let key_dotted = normalize_for_dedup(&dotted);
        assert_eq!(
            key_real, key_dotted,
            "logically-equal paths must share a dedup key"
        );
    }

    /// When canonicalize fails (path doesn't exist), the fallback must still
    /// produce a stable absolute key so duplicates dedup correctly. Pre-fix,
    /// this path returned `path.clone()` verbatim and a relative vs absolute
    /// spelling of the same missing directory escaped dedup.
    #[test]
    fn normalize_for_dedup_falls_back_for_missing_path() {
        // Non-existent path - canonicalize will fail.
        let missing_abs = PathBuf::from("/nonexistent/qartez/test/path");
        let k1 = normalize_for_dedup(&missing_abs);
        let k2 = normalize_for_dedup(&missing_abs);
        assert_eq!(k1, k2, "same input must yield same key");
        assert!(k1.is_absolute(), "fallback must produce an absolute key");

        // Relative path dedup: the cwd is stable within one process, so
        // std::path::absolute is deterministic. Two identical relative paths
        // must share a key.
        let missing_rel = PathBuf::from("does-not-exist-xyz");
        let rk1 = normalize_for_dedup(&missing_rel);
        let rk2 = normalize_for_dedup(&missing_rel);
        assert_eq!(rk1, rk2);
    }

    /// The pre-fix bug: when canonicalize fails, `unwrap_or_else(|_| root.clone())`
    /// kept the non-canonical form, so two different spellings of the same root
    /// both survived dedup. The new helper normalizes them through
    /// `std::path::absolute`, collapsing spellings even for missing paths.
    #[test]
    fn expand_roots_dedups_different_spellings_of_same_missing_path() {
        use std::collections::HashSet;
        // Relative vs absolute forms of "./foo/../foo" (all point at cwd/foo).
        let rel_a = PathBuf::from("foo");
        let rel_b = PathBuf::from("./foo");
        let rel_c = PathBuf::from("foo/../foo");

        let mut keys: HashSet<PathBuf> = HashSet::new();
        keys.insert(normalize_for_dedup(&rel_a));
        keys.insert(normalize_for_dedup(&rel_b));
        keys.insert(normalize_for_dedup(&rel_c));
        assert_eq!(
            keys.len(),
            1,
            "all three spellings must collapse to the same dedup key"
        );
    }
}
