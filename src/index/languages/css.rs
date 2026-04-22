use tree_sitter::{Language, Node};

use super::LanguageSupport;
use super::common::{self, children, node_text};
use crate::index::symbols::{ExtractedImport, ExtractedSymbol, ParseResult, SymbolKind};

pub struct CssSupport;

impl LanguageSupport for CssSupport {
    fn extensions(&self) -> &[&str] {
        &["css", "scss"]
    }

    fn language_name(&self) -> &str {
        "css"
    }

    fn tree_sitter_language(&self, _ext: &str) -> Language {
        Language::new(tree_sitter_css::LANGUAGE)
    }

    fn extract(&self, source: &[u8], tree: &tree_sitter::Tree) -> ParseResult {
        let mut symbols = Vec::new();
        let mut imports = Vec::new();
        let root = tree.root_node();
        extract_from_node(root, source, &mut symbols, &mut imports);
        ParseResult {
            symbols,
            imports,
            references: Vec::new(),
            ..Default::default()
        }
    }
}

fn extract_from_node(
    node: Node,
    source: &[u8],
    symbols: &mut Vec<ExtractedSymbol>,
    imports: &mut Vec<ExtractedImport>,
) {
    match node.kind() {
        "rule_set" => {
            extract_rule_set(node, source, symbols);
        }
        "keyframes_statement" => {
            if let Some(sym) = extract_keyframes(node, source) {
                symbols.push(sym);
            }
        }
        "import_statement" => {
            if let Some(imp) = extract_import(node, source) {
                imports.push(imp);
            }
            return;
        }
        "media_statement" => {
            if let Some(sym) = extract_media(node, source) {
                symbols.push(sym);
            }
            // Recurse into media block to find nested rules
            for child in children(node) {
                if child.kind() == "block" {
                    for block_child in children(child) {
                        extract_from_node(block_child, source, symbols, imports);
                    }
                }
            }
            return;
        }
        "declaration" => {
            if let Some(sym) = extract_custom_property(node, source) {
                symbols.push(sym);
            }
        }
        _ => {}
    }

    for child in children(node) {
        extract_from_node(child, source, symbols, imports);
    }
}

fn extract_rule_set(node: Node, source: &[u8], symbols: &mut Vec<ExtractedSymbol>) {
    for child in children(node) {
        if child.kind() == "selectors" {
            for selector in children(child) {
                extract_selectors(selector, node, source, symbols);
            }
            return;
        }
    }
}

fn extract_selectors(
    selector_node: Node,
    rule_node: Node,
    source: &[u8],
    symbols: &mut Vec<ExtractedSymbol>,
) {
    // Every top-level selector (a direct named child of the `selectors`
    // node) represents one rule target. Emit exactly ONE symbol per
    // top-level selector, using the selector's full source text as the
    // name so the clone detector doesn't see co-located class children as
    // duplicates:
    //
    //   * `.foo`                   -> 1 symbol `.foo` (class)
    //   * `#bar`                   -> 1 symbol `#bar` (id/variable)
    //   * `.a.b`                   -> 1 symbol `.a.b` (stacked class)
    //   * `.foo:hover`             -> 1 symbol `.foo:hover` (compound)
    //   * `.foo .bar path`         -> 1 symbol `.foo .bar path` (descendant)
    //   * `.foo > .bar`            -> 1 symbol `.foo > .bar` (child combinator)
    //   * `.a, .b`                 -> 2 symbols (one per top-level selector
    //                                  in the comma list)
    //
    // tree-sitter-css puts the `,` delimiter in a comma-selector list as
    // an anonymous child of `selectors`; skipping unnamed nodes filters
    // out punctuation without needing to enumerate kinds.
    //
    // The previous implementation recursed into descendant/compound nodes
    // and emitted a separate class/id symbol for every nested selector,
    // inflating the CSS symbol count (~2.4x on real projects) and feeding
    // the structural clone detector groups of co-located "duplicates"
    // that were all the same rule.
    if !selector_node.is_named() {
        return;
    }
    let kind = match selector_node.kind() {
        "id_selector" => SymbolKind::Variable,
        _ => SymbolKind::Class,
    };
    let name = normalize_selector_text(&node_text(selector_node, source));
    push_selector_symbol(name, kind, rule_node, source, symbols);
}

fn push_selector_symbol(
    name: String,
    kind: SymbolKind,
    rule_node: Node,
    source: &[u8],
    symbols: &mut Vec<ExtractedSymbol>,
) {
    if name.is_empty() {
        return;
    }
    symbols.push(ExtractedSymbol {
        name,
        kind,
        line_start: rule_node.start_position().row as u32 + 1,
        line_end: rule_node.end_position().row as u32 + 1,
        signature: extract_signature(rule_node, source),
        is_exported: true,
        parent_idx: None,
        unused_excluded: false,
        complexity: None,
        owner_type: None,
    });
}

fn normalize_selector_text(raw: &str) -> String {
    // Collapse tree-sitter-preserved whitespace (newlines, tabs, runs of
    // spaces) into a single space so compound selectors read the same
    // whether the author wrote them on one line or broke them across
    // several.
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_keyframes(node: Node, source: &[u8]) -> Option<ExtractedSymbol> {
    let name = children(node)
        .find(|c| c.kind() == "keyframes_name")
        .map(|n| node_text(n, source))?;
    if name.is_empty() {
        return None;
    }
    Some(ExtractedSymbol {
        name: format!("@keyframes {name}"),
        kind: SymbolKind::Function,
        line_start: node.start_position().row as u32 + 1,
        line_end: node.end_position().row as u32 + 1,
        signature: extract_signature(node, source),
        is_exported: true,
        parent_idx: None,
        unused_excluded: false,
        complexity: None,
        owner_type: None,
    })
}

fn extract_media(node: Node, source: &[u8]) -> Option<ExtractedSymbol> {
    let full = node_text(node, source);
    let first_line = full.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return None;
    }
    let sig = if let Some(brace_pos) = first_line.find('{') {
        first_line[..brace_pos].trim()
    } else {
        first_line
    };
    if sig.is_empty() {
        return None;
    }
    Some(ExtractedSymbol {
        name: sig.to_string(),
        kind: SymbolKind::Module,
        line_start: node.start_position().row as u32 + 1,
        line_end: node.end_position().row as u32 + 1,
        signature: Some(sig.to_string()),
        is_exported: true,
        parent_idx: None,
        unused_excluded: false,
        complexity: None,
        owner_type: None,
    })
}

fn extract_custom_property(node: Node, source: &[u8]) -> Option<ExtractedSymbol> {
    let prop_node = children(node).find(|c| c.kind() == "property_name")?;
    let name = node_text(prop_node, source);
    if !name.starts_with("--") {
        return None;
    }
    Some(ExtractedSymbol {
        name,
        kind: SymbolKind::Variable,
        line_start: node.start_position().row as u32 + 1,
        line_end: node.end_position().row as u32 + 1,
        signature: extract_signature(node, source),
        is_exported: true,
        parent_idx: None,
        unused_excluded: false,
        complexity: None,
        owner_type: None,
    })
}

fn extract_import(node: Node, source: &[u8]) -> Option<ExtractedImport> {
    for child in children(node) {
        match child.kind() {
            "string_value" => {
                let path = node_text(child, source);
                let unquoted = common::unquote(&path);
                if !unquoted.is_empty() {
                    return Some(ExtractedImport {
                        source: unquoted,
                        specifiers: vec![],
                        is_reexport: false,
                    });
                }
            }
            "call_expression" => {
                // url("path")
                for arg in children(child) {
                    if arg.kind() == "arguments" {
                        for val in children(arg) {
                            if val.kind() == "string_value" {
                                let path = node_text(val, source);
                                let unquoted = common::unquote(&path);
                                if !unquoted.is_empty() {
                                    return Some(ExtractedImport {
                                        source: unquoted,
                                        specifiers: vec![],
                                        is_reexport: false,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_signature(node: Node, source: &[u8]) -> Option<String> {
    common::brace_or_first_line_signature(node, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse_css(source: &str) -> ParseResult {
        let mut parser = Parser::new();
        let lang = Language::new(tree_sitter_css::LANGUAGE);
        parser.set_language(&lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let support = CssSupport;
        support.extract(source.as_bytes(), &tree)
    }

    #[test]
    fn test_class_selector() {
        let result = parse_css(".container { display: flex; }");
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, ".container");
        assert!(matches!(result.symbols[0].kind, SymbolKind::Class));
        assert!(result.symbols[0].is_exported);
    }

    #[test]
    fn test_id_selector() {
        let result = parse_css("#header { height: 60px; }");
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "#header");
        assert!(matches!(result.symbols[0].kind, SymbolKind::Variable));
    }

    #[test]
    fn test_keyframes() {
        let result = parse_css("@keyframes fadeIn { from { opacity: 0; } to { opacity: 1; } }");
        let kf: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Function))
            .collect();
        assert_eq!(kf.len(), 1);
        assert_eq!(kf[0].name, "@keyframes fadeIn");
    }

    #[test]
    fn test_custom_property() {
        let result = parse_css(":root { --primary-color: #333; }");
        let vars: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| s.name.starts_with("--"))
            .collect();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[0].name, "--primary-color");
    }

    #[test]
    fn test_import() {
        let result = parse_css("@import \"reset.css\";");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].source, "reset.css");
    }

    #[test]
    fn test_import_url() {
        let result = parse_css("@import url(\"fonts.css\");");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].source, "fonts.css");
    }

    #[test]
    fn test_empty_file() {
        let result = parse_css("");
        assert!(result.symbols.is_empty());
        assert!(result.imports.is_empty());
    }

    #[test]
    fn test_media_query() {
        let result = parse_css("@media (max-width: 768px) {\n  .mobile { display: block; }\n}");
        let media: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Module))
            .collect();
        assert_eq!(media.len(), 1);

        // Should also find nested class selector
        let classes: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Class))
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, ".mobile");
    }

    #[test]
    fn test_mixed_declarations() {
        let result = parse_css(
            r#"@import "base.css";

:root {
  --main-bg: #fff;
  --text-color: #333;
}

.header { background: var(--main-bg); }

#app { margin: 0 auto; }

@keyframes slideIn {
  from { transform: translateX(-100%); }
  to { transform: translateX(0); }
}
"#,
        );

        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"--main-bg"));
        assert!(names.contains(&"--text-color"));
        assert!(names.contains(&".header"));
        assert!(names.contains(&"#app"));
        assert!(names.contains(&"@keyframes slideIn"));

        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].source, "base.css");
    }

    #[test]
    fn test_descendant_selector_emits_one_symbol() {
        // `.hero__atlas .hero__arcs path { ... }` targets the `path`
        // element inside `.hero__arcs` inside `.hero__atlas`. That is ONE
        // rule, so the extractor should emit ONE symbol, not one per
        // nested class/tag.
        let result = parse_css(".hero__atlas .hero__arcs path { fill: red; }");
        assert_eq!(result.symbols.len(), 1, "descendant selector = one rule");
        assert_eq!(result.symbols[0].name, ".hero__atlas .hero__arcs path");
    }

    #[test]
    fn test_compound_selector_emits_one_symbol() {
        // Same rule, written with pseudo-class (`:hover`), combinator
        // (`>`), or stacked classes (`.a.b`) - still one rule, one
        // symbol each.
        let hover = parse_css(".btn:hover { opacity: 0.8; }");
        assert_eq!(hover.symbols.len(), 1);
        assert_eq!(hover.symbols[0].name, ".btn:hover");

        let child = parse_css(".card > .title { font-size: 16px; }");
        assert_eq!(child.symbols.len(), 1);
        assert_eq!(child.symbols[0].name, ".card > .title");

        let stacked = parse_css(".is-primary.is-disabled { opacity: 0.5; }");
        assert_eq!(stacked.symbols.len(), 1);
        assert_eq!(stacked.symbols[0].name, ".is-primary.is-disabled");
    }

    #[test]
    fn test_selector_list_emits_one_symbol_per_entry() {
        // Comma-separated selectors DO target distinct things - each entry
        // gets its own symbol so callers can navigate to a specific
        // class/id.
        let result = parse_css(".a, .b, #c { display: none; }");
        let names: Vec<&str> = result.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(result.symbols.len(), 3);
        assert!(names.contains(&".a"));
        assert!(names.contains(&".b"));
        assert!(names.contains(&"#c"));
    }

    #[test]
    fn test_compound_selector_normalizes_whitespace() {
        // Authors routinely wrap long selector lists across multiple
        // lines. The emitted symbol name should collapse any whitespace
        // (newlines, tabs, runs of spaces) into a single space so the
        // symbol reads identically regardless of formatting.
        let src = ".outer\n  .middle\n  .inner { color: red; }";
        let result = parse_css(src);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, ".outer .middle .inner");
    }
}
