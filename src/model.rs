use std::collections::BTreeMap;

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Serialize};

pub const INDEX_VERSION: u32 = 8;

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct IndexMetadata {
    pub version: u32,
    pub tool_version: String,
    pub repo_root_hint: String,
    pub git_head: Option<String>,
    pub rust_file_list_hash: String,
    pub tracked_file_list_hash: String,
    pub cargo_metadata_hash: String,
    pub generated_at_unix_ms: u128,
    pub files: Vec<FileNode>,
    pub items: Vec<ItemNode>,
    pub packages: Vec<PackageNode>,
    pub targets: Vec<TargetNode>,
    pub package_dependencies: Vec<PackageDependency>,
    pub module_declarations: Vec<ModuleDeclarationNode>,
    pub source_dependencies: Vec<SourceDependencyNode>,
    pub edges: Vec<Edge>,
}

impl IndexMetadata {
    pub fn sorted(mut self) -> Self {
        self.files.sort_by(|a, b| a.id.cmp(&b.id));
        self.items.sort_by(|a, b| a.id.cmp(&b.id));
        self.packages.sort_by(|a, b| a.id.cmp(&b.id));
        self.targets.sort_by(|a, b| a.id.cmp(&b.id));
        self.package_dependencies.sort_by(|a, b| {
            a.package_id
                .cmp(&b.package_id)
                .then_with(|| a.depends_on_package_id.cmp(&b.depends_on_package_id))
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.target.cmp(&b.target))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.depends_on_path.cmp(&b.depends_on_path))
        });
        self.module_declarations.sort_by(|a, b| {
            a.from_item_id
                .cmp(&b.from_item_id)
                .then_with(|| a.target_paths.cmp(&b.target_paths))
        });
        self.source_dependencies.sort_by(|a, b| {
            a.source_path
                .cmp(&b.source_path)
                .then_with(|| a.target_path.cmp(&b.target_path))
                .then_with(|| a.kind.cmp(&b.kind))
        });
        self.edges.sort_by(|a, b| {
            a.from
                .cmp(&b.from)
                .then_with(|| a.to.cmp(&b.to))
                .then_with(|| a.kind.cmp(&b.kind))
        });
        self
    }

    pub fn item_by_id(&self) -> BTreeMap<String, &ItemNode> {
        self.items
            .iter()
            .map(|item| (item.id.clone(), item))
            .collect()
    }
}

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct SourceDependencyNode {
    pub source_path: String,
    pub target_path: String,
    pub kind: SourceDependencyKind,
}

#[derive(
    Archive,
    RkyvDeserialize,
    RkyvSerialize,
    Clone,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceDependencyKind {
    PathModule,
    Include,
    IncludeStr,
    IncludeBytes,
}

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct ModuleDeclarationNode {
    pub file_id: String,
    pub from_item_id: String,
    pub target_paths: Vec<String>,
}

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct FileNode {
    pub id: String,
    pub path: String,
    pub hash: String,
    pub byte_len: u64,
}

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct ItemNode {
    pub id: String,
    pub file_id: String,
    pub kind: ItemKind,
    pub name: String,
    pub visibility: String,
    pub span: SpanRange,
    pub parent_item_id: Option<String>,
    pub is_test: bool,
}

impl ItemNode {
    pub fn contains_line_range(&self, start_line: usize, end_line: usize) -> bool {
        self.span.start_line <= end_line && self.span.end_line >= start_line
    }
}

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct PackageNode {
    pub id: String,
    pub name: String,
    pub workspace: String,
    pub workspace_path: String,
    pub manifest_path: String,
    pub root_path: String,
}

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct TargetNode {
    pub id: String,
    pub package_id: String,
    pub name: String,
    pub kind: Vec<String>,
    pub test: bool,
    pub src_path: String,
}

impl TargetNode {
    pub fn is_bin(&self) -> bool {
        self.kind.iter().any(|kind| kind == "bin")
    }

    pub fn is_test(&self) -> bool {
        self.kind.iter().any(|kind| kind == "test")
    }
}

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct PackageDependency {
    pub package_id: String,
    pub depends_on_package_id: String,
    pub name: String,
    pub depends_on_path: String,
    pub kind: String,
    pub target: Option<String>,
}

#[derive(
    Archive,
    RkyvDeserialize,
    RkyvSerialize,
    Clone,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
)]
pub enum ItemKind {
    Const,
    Enum,
    Fn,
    ForeignMod,
    Impl,
    Macro,
    Mod,
    Static,
    Struct,
    Trait,
    TraitAlias,
    Type,
    Union,
    Use,
    Verbatim,
}

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct SpanRange {
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

#[derive(
    Archive, RkyvDeserialize, RkyvSerialize, Clone, Debug, Deserialize, PartialEq, Serialize,
)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(
    Archive,
    RkyvDeserialize,
    RkyvSerialize,
    Clone,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
)]
pub enum EdgeKind {
    Contains,
    ModuleDeclares,
}
