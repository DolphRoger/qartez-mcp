//! Path-based detection of test files.
//!
//! Shared by `graph::security::scan` (to exclude test files from the
//! security report when `include_tests=false`) and `server::helpers`
//! (test-gaps / refactor-plan / diff-impact summaries). Both callers
//! classify by file path; keeping one source of truth avoids the drift
//! that let `quality_tests.rs` leak into the security scan under the
//! narrower predicate.
//!
//! The rules intentionally err on the side of treating a file as a test:
//! the downstream cost of a missed test file (noise in a report) is
//! higher than the cost of excluding a source file that happened to
//! follow a test naming convention.

/// True when `path` points at a file that should be treated as test code.
///
/// Covers the conventional layouts across the languages Qartez indexes:
///
/// * Directory-based: anything under `tests/`, `test/`, `benches/`,
///   `__tests__/`, or `spec/`.
/// * Rust: filenames `test.rs`, `tests.rs`, or ending in `_test.rs` /
///   `_tests.rs`.
/// * JS / TS: `.test.{js,jsx,ts,tsx}` / `.spec.{js,jsx,ts,tsx}`.
/// * Go / Dart: `_test.go` / `_test.dart`.
/// * Python: `_test.py` and `test_*.py`.
/// * Java / Kotlin: `Test.java` / `Tests.java` / `Test.kt` / `Tests.kt`.
/// * Ruby: `_spec.rb`.
/// * C#: `Test.cs` / `Tests.cs`.
///
/// The function is purely path-based. It does NOT inspect file contents
/// or module relationships. Inline `#[cfg(test)] mod { ... }` blocks in
/// production files are filtered separately via tree-sitter in the
/// security scanner; external `#[cfg(test)] mod foo;` declarations are
/// picked up here as long as `foo.rs` follows one of the conventional
/// naming patterns above.
pub(crate) fn is_test_path(path: &str) -> bool {
    const TEST_DIR_PREFIXES: &[&str] = &["tests/", "test/", "benches/", "__tests__/", "spec/"];
    const TEST_DIR_SUBSTRINGS: &[&str] =
        &["/tests/", "/test/", "/benches/", "/__tests__/", "/spec/"];
    const TEST_FILE_EXACT: &[&str] = &["test.rs", "tests.rs"];
    const TEST_FILE_SUFFIXES: &[&str] = &[
        "_test.rs",
        "_tests.rs",
        "_test.go",
        "_test.dart",
        ".test.ts",
        ".spec.ts",
        ".test.tsx",
        ".spec.tsx",
        ".test.js",
        ".spec.js",
        ".test.jsx",
        ".spec.jsx",
        "_test.py",
        "Test.java",
        "Tests.java",
        "Test.kt",
        "Tests.kt",
        "_spec.rb",
        "Test.cs",
        "Tests.cs",
    ];

    if TEST_DIR_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return true;
    }
    if TEST_DIR_SUBSTRINGS.iter().any(|p| path.contains(p)) {
        return true;
    }
    let Some(name) = path.rsplit('/').next() else {
        return false;
    };
    if TEST_FILE_EXACT.contains(&name) {
        return true;
    }
    if TEST_FILE_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    name.starts_with("test_") && name.ends_with(".py")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_prefixes() {
        assert!(is_test_path("tests/foo.rs"));
        assert!(is_test_path("test/bar.go"));
        assert!(is_test_path("benches/baz.rs"));
        assert!(is_test_path("__tests__/snap.tsx"));
        assert!(is_test_path("spec/widget_spec.rb"));
    }

    #[test]
    fn dir_substrings() {
        assert!(is_test_path("src/tests/util.rs"));
        assert!(is_test_path("crate/test/helpers.go"));
        assert!(is_test_path("pkg/__tests__/index.ts"));
    }

    #[test]
    fn rust_file_names() {
        assert!(is_test_path("src/lib/test.rs"));
        assert!(is_test_path("src/lib/tests.rs"));
        assert!(is_test_path("src/server/quality_tests.rs"));
        assert!(is_test_path("src/foo_test.rs"));
    }

    #[test]
    fn js_ts_suffixes() {
        assert!(is_test_path("components/foo.test.ts"));
        assert!(is_test_path("components/foo.spec.tsx"));
        assert!(is_test_path("components/foo.test.js"));
        assert!(is_test_path("components/foo.spec.jsx"));
    }

    #[test]
    fn python_patterns() {
        assert!(is_test_path("pkg/mod_test.py"));
        assert!(is_test_path("pkg/test_mod.py"));
        assert!(!is_test_path("pkg/tester.py"));
    }

    #[test]
    fn production_paths() {
        assert!(!is_test_path("src/main.rs"));
        assert!(!is_test_path("src/server/mod.rs"));
        assert!(!is_test_path("src/graph/security.rs"));
        assert!(!is_test_path("test_data.json"));
    }

    #[test]
    fn narrow_predicate_was_missing_quality_tests_rs() {
        // Regression guard for the divergent-predicate bug: the older
        // graph/security.rs variant only matched `_test.` (no `s`) and
        // let quality_tests.rs leak findings into the main report.
        assert!(is_test_path("qartez-public/src/server/quality_tests.rs"));
    }
}
