use std::path::{Component, Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::model::{
    Edge, EdgeKind, FileNode, ItemKind, ItemNode, SourceDependencyKind, SourceDependencyNode,
    SpanRange,
};

#[derive(Clone, Debug)]
pub struct ParsedFile {
    pub items: Vec<ItemNode>,
    pub edges: Vec<Edge>,
    pub module_declarations: Vec<ModuleDeclaration>,
    pub source_dependencies: Vec<SourceDependencyNode>,
}

#[derive(Clone, Debug)]
pub struct ModuleDeclaration {
    pub from_item_id: String,
    pub target_paths: Vec<String>,
}

pub fn parse_rust_file(
    repo_relative_path: &str,
    file_node: &FileNode,
    source: &str,
) -> syn::Result<ParsedFile> {
    let syntax = syn::parse_file(source)?;
    let mut collector = Collector {
        repo_relative_path,
        file_node,
        file_parent: Path::new(repo_relative_path).parent(),
        parent_item_id: None,
        parent_path: Vec::new(),
        cfg_test_depth: 0,
        items: Vec::new(),
        edges: Vec::new(),
        module_declarations: Vec::new(),
        source_dependencies: Vec::new(),
    };

    for item in &syntax.items {
        collector.collect_item(item);
    }
    let mut include_collector = IncludeCollector {
        source_path: repo_relative_path,
        file_parent: Path::new(repo_relative_path).parent(),
        dependencies: Vec::new(),
    };
    include_collector.visit_file(&syntax);

    Ok(ParsedFile {
        items: collector.items,
        edges: collector.edges,
        module_declarations: collector.module_declarations,
        source_dependencies: collector
            .source_dependencies
            .into_iter()
            .chain(include_collector.dependencies)
            .collect(),
    })
}

struct Collector<'a> {
    repo_relative_path: &'a str,
    file_node: &'a FileNode,
    file_parent: Option<&'a Path>,
    parent_item_id: Option<String>,
    parent_path: Vec<String>,
    cfg_test_depth: usize,
    items: Vec<ItemNode>,
    edges: Vec<Edge>,
    module_declarations: Vec<ModuleDeclaration>,
    source_dependencies: Vec<SourceDependencyNode>,
}

impl Collector<'_> {
    fn collect_item(&mut self, item: &syn::Item) {
        let Some((kind, local_name, visibility)) = item_descriptor(item) else {
            return;
        };

        let attrs = item_attrs(item);
        let in_cfg_test = has_cfg_test(attrs);
        let item_id = self.with_cfg_test(in_cfg_test, |collector| {
            collector.push_item(kind, local_name.clone(), visibility, item.span(), attrs)
        });

        if let syn::Item::Mod(item_mod) = item {
            if let Some((_, items)) = &item_mod.content {
                self.with_cfg_test(in_cfg_test, |collector| {
                    collector.with_parent(item_id, local_name, |collector| {
                        for child in items {
                            collector.collect_item(child);
                        }
                    });
                });
            } else {
                let (target_paths, explicit_path) = self.resolve_module_paths(item_mod);
                if !target_paths.is_empty() {
                    self.module_declarations.push(ModuleDeclaration {
                        from_item_id: item_id,
                        target_paths: target_paths.clone(),
                    });
                    if explicit_path {
                        self.source_dependencies.push(SourceDependencyNode {
                            source_path: self.repo_relative_path.to_string(),
                            target_path: target_paths[0].clone(),
                            kind: SourceDependencyKind::PathModule,
                        });
                    }
                }
            }
        } else if let syn::Item::Impl(item_impl) = item {
            self.with_cfg_test(in_cfg_test, |collector| {
                collector.with_parent(item_id, local_name, |collector| {
                    for child in &item_impl.items {
                        collector.collect_impl_item(child);
                    }
                });
            });
        } else if let syn::Item::Trait(item_trait) = item {
            self.with_cfg_test(in_cfg_test, |collector| {
                collector.with_parent(item_id, local_name, |collector| {
                    for child in &item_trait.items {
                        collector.collect_trait_item(child);
                    }
                });
            });
        }
    }

    fn collect_impl_item(&mut self, item: &syn::ImplItem) {
        let (kind, local_name) = match item {
            syn::ImplItem::Const(item) => (ItemKind::Const, item.ident.to_string()),
            syn::ImplItem::Fn(item) => (ItemKind::Fn, item.sig.ident.to_string()),
            syn::ImplItem::Macro(item) => (
                ItemKind::Macro,
                item.mac
                    .path
                    .segments
                    .last()
                    .map_or_else(|| "macro".to_string(), |segment| segment.ident.to_string()),
            ),
            syn::ImplItem::Type(item) => (ItemKind::Type, item.ident.to_string()),
            syn::ImplItem::Verbatim(_) => (ItemKind::Verbatim, "verbatim".to_string()),
            _ => return,
        };

        self.push_item(
            kind,
            local_name,
            "inherited".to_string(),
            item.span(),
            impl_item_attrs(item),
        );
    }

    fn collect_trait_item(&mut self, item: &syn::TraitItem) {
        let (kind, local_name) = match item {
            syn::TraitItem::Const(item) => (ItemKind::Const, item.ident.to_string()),
            syn::TraitItem::Fn(item) => (ItemKind::Fn, item.sig.ident.to_string()),
            syn::TraitItem::Macro(item) => (
                ItemKind::Macro,
                item.mac
                    .path
                    .segments
                    .last()
                    .map_or_else(|| "macro".to_string(), |segment| segment.ident.to_string()),
            ),
            syn::TraitItem::Type(item) => (ItemKind::Type, item.ident.to_string()),
            syn::TraitItem::Verbatim(_) => (ItemKind::Verbatim, "verbatim".to_string()),
            _ => return,
        };

        self.push_item(
            kind,
            local_name,
            "inherited".to_string(),
            item.span(),
            trait_item_attrs(item),
        );
    }

    fn push_item(
        &mut self,
        kind: ItemKind,
        local_name: String,
        visibility: String,
        span: proc_macro2::Span,
        attrs: &[syn::Attribute],
    ) -> String {
        let span = span_range(span);
        let mut item_path = self.parent_path.clone();
        item_path.push(local_name);
        let qualified_name = item_path.join("::");
        let id = item_id(self.repo_relative_path, &kind, &qualified_name, &span);

        let item = ItemNode {
            id: id.clone(),
            file_id: self.file_node.id.clone(),
            kind,
            name: qualified_name,
            visibility,
            span,
            parent_item_id: self.parent_item_id.clone(),
            is_test: self.cfg_test_depth > 0 || has_test_attr(attrs),
        };

        let from = self
            .parent_item_id
            .clone()
            .unwrap_or_else(|| self.file_node.id.clone());
        self.edges.push(Edge {
            from,
            to: id.clone(),
            kind: EdgeKind::Contains,
        });
        self.items.push(item);
        id
    }

    fn with_cfg_test<T>(&mut self, enabled: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        if enabled {
            self.cfg_test_depth += 1;
        }
        let result = f(self);
        if enabled {
            self.cfg_test_depth -= 1;
        }
        result
    }

    fn with_parent(
        &mut self,
        parent_item_id: String,
        path_segment: String,
        f: impl FnOnce(&mut Self),
    ) {
        let previous_parent = self.parent_item_id.replace(parent_item_id);
        self.parent_path.push(path_segment);
        f(self);
        self.parent_path.pop();
        self.parent_item_id = previous_parent;
    }

    fn resolve_module_paths(&self, item_mod: &syn::ItemMod) -> (Vec<String>, bool) {
        let Some(parent) = self.file_parent else {
            return (Vec::new(), false);
        };
        if let Some(explicit) = path_attribute(&item_mod.attrs) {
            let target = normalize_lexical_path(&parent.join(explicit))
                .and_then(|target| path_to_repo_string(&target));
            return (target.into_iter().collect(), true);
        }
        let module_name = item_mod.ident.to_string();
        let paths = [
            parent.join(format!("{module_name}.rs")),
            parent.join(module_name).join("mod.rs"),
        ]
        .iter()
        .filter_map(|path| path_to_repo_string(path))
        .collect::<Vec<_>>();
        (paths, false)
    }
}

struct IncludeCollector<'a> {
    source_path: &'a str,
    file_parent: Option<&'a Path>,
    dependencies: Vec<SourceDependencyNode>,
}

impl<'ast> Visit<'ast> for IncludeCollector<'_> {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let kind = match mac
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string())
        {
            Some(name) if name == "include" => SourceDependencyKind::Include,
            Some(name) if name == "include_str" => SourceDependencyKind::IncludeStr,
            Some(name) if name == "include_bytes" => SourceDependencyKind::IncludeBytes,
            _ => {
                syn::visit::visit_macro(self, mac);
                return;
            }
        };
        if let (Some(parent), Ok(path)) = (
            self.file_parent,
            syn::parse2::<syn::LitStr>(mac.tokens.clone()),
        ) {
            let target_path = normalize_lexical_path(&parent.join(path.value()))
                .and_then(|target| path_to_repo_string(&target));
            if let Some(target_path) = target_path {
                self.dependencies.push(SourceDependencyNode {
                    source_path: self.source_path.to_string(),
                    target_path,
                    kind,
                });
            }
        }
        syn::visit::visit_macro(self, mac);
    }
}

fn path_attribute(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| {
        if !attr.path().is_ident("path") {
            return None;
        }
        let syn::Meta::NameValue(value) = &attr.meta else {
            return None;
        };
        let syn::Expr::Lit(expression) = &value.value else {
            return None;
        };
        let syn::Lit::Str(path) = &expression.lit else {
            return None;
        };
        Some(path.value())
    })
}

fn normalize_lexical_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(component) => normalized.push(component),
            Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    Some(normalized)
}

fn item_descriptor(item: &syn::Item) -> Option<(ItemKind, String, String)> {
    Some(match item {
        syn::Item::Const(item) => (
            ItemKind::Const,
            item.ident.to_string(),
            visibility(&item.vis),
        ),
        syn::Item::Enum(item) => (
            ItemKind::Enum,
            item.ident.to_string(),
            visibility(&item.vis),
        ),
        syn::Item::Fn(item) => (
            ItemKind::Fn,
            item.sig.ident.to_string(),
            visibility(&item.vis),
        ),
        syn::Item::ForeignMod(_) => (
            ItemKind::ForeignMod,
            "foreign_mod".to_string(),
            "inherited".to_string(),
        ),
        syn::Item::Impl(item) => (
            ItemKind::Impl,
            format!("impl@{}", line_col_key(item.impl_token.span)),
            "inherited".to_string(),
        ),
        syn::Item::Macro(item) => (
            ItemKind::Macro,
            item.mac
                .path
                .segments
                .last()
                .map_or_else(|| "macro".to_string(), |segment| segment.ident.to_string()),
            "inherited".to_string(),
        ),
        syn::Item::Mod(item) => (ItemKind::Mod, item.ident.to_string(), visibility(&item.vis)),
        syn::Item::Static(item) => (
            ItemKind::Static,
            item.ident.to_string(),
            visibility(&item.vis),
        ),
        syn::Item::Struct(item) => (
            ItemKind::Struct,
            item.ident.to_string(),
            visibility(&item.vis),
        ),
        syn::Item::Trait(item) => (
            ItemKind::Trait,
            item.ident.to_string(),
            visibility(&item.vis),
        ),
        syn::Item::TraitAlias(item) => (
            ItemKind::TraitAlias,
            item.ident.to_string(),
            visibility(&item.vis),
        ),
        syn::Item::Type(item) => (
            ItemKind::Type,
            item.ident.to_string(),
            visibility(&item.vis),
        ),
        syn::Item::Union(item) => (
            ItemKind::Union,
            item.ident.to_string(),
            visibility(&item.vis),
        ),
        syn::Item::Use(item) => (
            ItemKind::Use,
            format!("use@{}", line_col_key(item.use_token.span)),
            visibility(&item.vis),
        ),
        syn::Item::Verbatim(_) => (
            ItemKind::Verbatim,
            "verbatim".to_string(),
            "inherited".to_string(),
        ),
        _ => return None,
    })
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        syn::Item::Verbatim(_) => &[],
        _ => &[],
    }
}

fn impl_item_attrs(item: &syn::ImplItem) -> &[syn::Attribute] {
    match item {
        syn::ImplItem::Const(item) => &item.attrs,
        syn::ImplItem::Fn(item) => &item.attrs,
        syn::ImplItem::Macro(item) => &item.attrs,
        syn::ImplItem::Type(item) => &item.attrs,
        syn::ImplItem::Verbatim(_) => &[],
        _ => &[],
    }
}

fn trait_item_attrs(item: &syn::TraitItem) -> &[syn::Attribute] {
    match item {
        syn::TraitItem::Const(item) => &item.attrs,
        syn::TraitItem::Fn(item) => &item.attrs,
        syn::TraitItem::Macro(item) => &item.attrs,
        syn::TraitItem::Type(item) => &item.attrs,
        syn::TraitItem::Verbatim(_) => &[],
        _ => &[],
    }
}

fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();
        path.is_ident("test")
            || path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "test")
    })
}

fn has_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        match &attr.meta {
            syn::Meta::List(list) => list.tokens.to_string().contains("test"),
            _ => false,
        }
    })
}

fn visibility(vis: &syn::Visibility) -> String {
    match vis {
        syn::Visibility::Public(_) => "pub".to_string(),
        syn::Visibility::Restricted(restricted) => {
            if restricted.in_token.is_some() {
                "pub(in ...)".to_string()
            } else {
                "pub(...)".to_string()
            }
        }
        syn::Visibility::Inherited => "private".to_string(),
    }
}

fn span_range(span: proc_macro2::Span) -> SpanRange {
    let start = span.start();
    let end = span.end();
    SpanRange {
        start_line: start.line,
        start_column: start.column,
        end_line: end.line,
        end_column: end.column,
    }
}

fn line_col_key(span: proc_macro2::Span) -> String {
    let start = span.start();
    format!("{}:{}", start.line, start.column)
}

fn item_id(path: &str, kind: &ItemKind, name: &str, span: &SpanRange) -> String {
    blake3::hash(
        format!(
            "item\0{path}\0{kind:?}\0{name}\0{}:{}-{}:{}",
            span.start_line, span.start_column, span.end_line, span.end_column
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

fn path_to_repo_string(path: &Path) -> Option<String> {
    path.to_str().map(|path| path.replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileNode;

    fn file_node() -> FileNode {
        FileNode {
            id: "file".to_string(),
            path: "src/lib.rs".to_string(),
            hash: "hash".to_string(),
            byte_len: 0,
        }
    }

    #[test]
    fn parses_common_item_kinds_and_spans() {
        let source = r#"
mod nested {
    pub struct Thing;
    enum Choice { A }
    trait DoIt { fn go(&self); }
    impl Thing { pub fn run(&self) {} }
    #[test]
    fn checks_it() {}
}
const ANSWER: u8 = 42;
macro_rules! m { () => {} }
"#;

        let parsed = parse_rust_file("src/lib.rs", &file_node(), source).unwrap();
        let names: Vec<_> = parsed.items.iter().map(|item| item.name.as_str()).collect();

        assert!(names.contains(&"nested"));
        assert!(names.contains(&"nested::Thing"));
        assert!(names.contains(&"nested::Choice"));
        assert!(names.contains(&"nested::DoIt"));
        assert!(names.iter().any(|name| name.starts_with("nested::impl@")));
        assert!(names.contains(&"nested::checks_it"));
        assert!(names.contains(&"ANSWER"));
        assert!(
            parsed
                .items
                .iter()
                .any(|item| item.name == "nested::checks_it"
                    && item.kind == ItemKind::Fn
                    && item.is_test)
        );
        assert!(
            parsed
                .items
                .iter()
                .any(|item| item.contains_line_range(3, 3))
        );
    }

    #[test]
    fn marks_cfg_test_module_items_as_tests() {
        let source = r#"
#[cfg(test)]
mod tests {
    async fn helper() {}

    #[tokio::test]
    async fn async_case() {}
}
"#;

        let parsed = parse_rust_file("src/lib.rs", &file_node(), source).unwrap();

        assert!(
            parsed
                .items
                .iter()
                .any(|item| item.name == "tests::helper" && item.is_test)
        );
        assert!(
            parsed
                .items
                .iter()
                .any(|item| item.name == "tests::async_case" && item.is_test)
        );
    }

    #[test]
    fn ids_are_deterministic() {
        let source = "pub fn stable() {}\n";
        let first = parse_rust_file("src/lib.rs", &file_node(), source).unwrap();
        let second = parse_rust_file("src/lib.rs", &file_node(), source).unwrap();

        assert_eq!(first.items[0].id, second.items[0].id);
    }

    #[test]
    fn records_mod_declarations() {
        let parsed = parse_rust_file("src/lib.rs", &file_node(), "mod foo;\n").unwrap();

        assert_eq!(parsed.module_declarations.len(), 1);
        assert_eq!(parsed.module_declarations[0].target_paths[0], "src/foo.rs");
    }

    #[test]
    fn records_explicit_path_modules() {
        let parsed = parse_rust_file(
            "src/lib.rs",
            &file_node(),
            "#[path = \"generated/types.rs\"]\nmod types;\n",
        )
        .unwrap();

        assert_eq!(
            parsed.module_declarations[0].target_paths,
            vec!["src/generated/types.rs"]
        );
        assert_eq!(parsed.source_dependencies.len(), 1);
        assert_eq!(
            parsed.source_dependencies[0].kind,
            SourceDependencyKind::PathModule
        );
    }

    #[test]
    fn records_include_dependencies_anywhere_in_source() {
        let parsed = parse_rust_file(
            "src/lib.rs",
            &file_node(),
            r#"
const SCHEMA: &str = include_str!("../schemas/api.json");
const BYTES: &[u8] = include_bytes!("fixtures/data.bin");
include!("generated.rs");
"#,
        )
        .unwrap();

        let dependencies = parsed
            .source_dependencies
            .iter()
            .map(|dependency| (dependency.target_path.as_str(), &dependency.kind))
            .collect::<Vec<_>>();
        assert!(dependencies.contains(&("schemas/api.json", &SourceDependencyKind::IncludeStr)));
        assert!(
            dependencies.contains(&("src/fixtures/data.bin", &SourceDependencyKind::IncludeBytes))
        );
        assert!(dependencies.contains(&("src/generated.rs", &SourceDependencyKind::Include)));
    }

    #[test]
    fn ignores_source_dependencies_outside_repository() {
        let parsed = parse_rust_file(
            "src/lib.rs",
            &file_node(),
            r#"
#[path = "../../external.rs"]
mod external;
const SCHEMA: &str = include_str!("../../schemas/api.json");
"#,
        )
        .unwrap();

        assert!(parsed.module_declarations.is_empty());
        assert!(parsed.source_dependencies.is_empty());
    }
}
