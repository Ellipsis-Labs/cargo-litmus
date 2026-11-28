//! Attribution of `Cargo.lock` changes to the path packages they can affect.
//!
//! A lockfile records every resolved package with its version, source,
//! checksum, and dependency list. Comparing two lockfiles identifies the locked
//! packages that were added, removed, or changed, and each lockfile's own
//! dependency graph identifies the path packages (workspace members and path
//! dependencies) whose resolved closure contains one of them. A lockfile does
//! not record features, so feature changes must come from manifest analysis.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use toml::{Table, Value};

/// Lockfile format versions whose dependency references are resolved here.
/// Older formats store checksums in a separate `[metadata]` table and spell
/// dependency references differently.
const SUPPORTED_FORMAT_VERSIONS: &[i64] = &[3, 4];

/// Why a lockfile change cannot be attributed to specific packages.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum LockfileUncertainty {
    #[error("{side} lockfile is not valid: {message}")]
    Invalid { side: &'static str, message: String },
    #[error("{side} lockfile format version {version} is not supported")]
    UnsupportedFormat { side: &'static str, version: String },
    #[error("lockfile format version changed")]
    FormatChanged,
    #[error("lockfile tables other than [[package]] changed")]
    NonPackageTablesChanged,
    #[error("{side} lockfile locks {package} more than once")]
    DuplicatePackage { side: &'static str, package: String },
    #[error(
        "{side} lockfile dependency {dependency:?} of {package} does not resolve to exactly one \
         locked package"
    )]
    UnresolvedDependency {
        side: &'static str,
        package: String,
        dependency: String,
    },
}

/// The packages a lockfile change can affect.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LockfileChange {
    /// Locked packages that were added, removed, or changed, as `name version`.
    pub changed_packages: BTreeSet<String>,
    /// Names of path packages whose own locked entry changed.
    pub changed_path_packages: BTreeSet<String>,
    /// Names of path packages whose resolved dependency closure, in the base or
    /// the head graph, contains a changed package. Includes
    /// `changed_path_packages`.
    pub affected_path_packages: BTreeSet<String>,
}

impl LockfileChange {
    /// Add the packages another change can affect.
    pub fn merge(&mut self, other: Self) {
        self.changed_packages.extend(other.changed_packages);
        self.changed_path_packages
            .extend(other.changed_path_packages);
        self.affected_path_packages
            .extend(other.affected_path_packages);
    }
}

/// Compare two lockfiles of the same workspace.
///
/// Removed packages are traced through the base graph and added or changed
/// packages through the head graph, so a dependent is found on whichever side
/// still records the edge.
pub fn lockfile_change(base: &str, head: &str) -> Result<LockfileChange, LockfileUncertainty> {
    let base = Lockfile::parse(base, "base")?;
    let head = Lockfile::parse(head, "head")?;
    if base.format != head.format {
        return Err(LockfileUncertainty::FormatChanged);
    }
    if base.other_tables != head.other_tables {
        return Err(LockfileUncertainty::NonPackageTablesChanged);
    }

    let changed = base
        .packages
        .keys()
        .chain(head.packages.keys())
        .filter(|key| base.packages.get(*key) != head.packages.get(*key))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut affected_path_packages = base.path_packages_reaching(&changed);
    affected_path_packages.extend(head.path_packages_reaching(&changed));
    Ok(LockfileChange {
        changed_packages: changed.iter().map(PackageKey::label).collect(),
        changed_path_packages: changed
            .iter()
            .filter(|key| key.source.is_none())
            .map(|key| key.name.clone())
            .collect(),
        affected_path_packages,
    })
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PackageKey {
    name: String,
    version: String,
    source: Option<String>,
}

impl PackageKey {
    fn label(&self) -> String {
        format!("{} {}", self.name, self.version)
    }
}

struct Lockfile {
    format: i64,
    other_tables: Table,
    packages: BTreeMap<PackageKey, Table>,
    dependents: BTreeMap<PackageKey, Vec<PackageKey>>,
}

impl Lockfile {
    fn parse(contents: &str, side: &'static str) -> Result<Self, LockfileUncertainty> {
        let invalid = |message: String| LockfileUncertainty::Invalid { side, message };
        let mut table =
            toml::from_str::<Table>(contents).map_err(|error| invalid(error.to_string()))?;
        let format = match table.remove("version") {
            Some(Value::Integer(version)) if SUPPORTED_FORMAT_VERSIONS.contains(&version) => {
                version
            }
            version => {
                return Err(LockfileUncertainty::UnsupportedFormat {
                    side,
                    version: version.map_or_else(|| "absent".to_string(), |v| v.to_string()),
                });
            }
        };
        let entries = match table.remove("package") {
            None => Vec::new(),
            Some(Value::Array(entries)) => entries,
            Some(_) => return Err(invalid("`package` is not an array".to_string())),
        };

        let mut packages = BTreeMap::new();
        for entry in entries {
            let Value::Table(entry) = entry else {
                return Err(invalid("a `package` entry is not a table".to_string()));
            };
            let key = package_key(&entry).ok_or_else(|| {
                invalid("a `package` entry lacks a string name or version".to_string())
            })?;
            if packages.contains_key(&key) {
                return Err(LockfileUncertainty::DuplicatePackage {
                    side,
                    package: key.label(),
                });
            }
            packages.insert(key, entry);
        }

        let mut keys_by_name = BTreeMap::<&str, Vec<&PackageKey>>::new();
        for key in packages.keys() {
            keys_by_name.entry(&key.name).or_default().push(key);
        }
        let mut dependents = BTreeMap::<PackageKey, Vec<PackageKey>>::new();
        for (key, entry) in &packages {
            let references = match entry.get("dependencies") {
                None => &Vec::new(),
                Some(references) => references.as_array().ok_or_else(|| {
                    invalid(format!("dependencies of {} are not an array", key.label()))
                })?,
            };
            // Under a `[replace]` override the replaced package lists no
            // dependencies and names its stand-in under `replace`, so that
            // reference is the only edge from its dependents to the stand-in.
            for reference in references.iter().chain(entry.get("replace")) {
                let unresolved = || LockfileUncertainty::UnresolvedDependency {
                    side,
                    package: key.label(),
                    dependency: reference.to_string(),
                };
                let dependency = reference
                    .as_str()
                    .and_then(|reference| resolve_reference(&keys_by_name, reference))
                    .ok_or_else(unresolved)?;
                dependents
                    .entry(dependency.clone())
                    .or_default()
                    .push(key.clone());
            }
        }

        Ok(Self {
            format,
            other_tables: table,
            packages,
            dependents,
        })
    }

    fn path_packages_reaching(&self, changed: &BTreeSet<PackageKey>) -> BTreeSet<String> {
        let mut visited = changed
            .iter()
            .filter(|key| self.packages.contains_key(*key))
            .collect::<BTreeSet<_>>();
        let mut pending = visited.iter().copied().collect::<Vec<_>>();
        while let Some(current) = pending.pop() {
            for dependent in self.dependents.get(current).into_iter().flatten() {
                if visited.insert(dependent) {
                    pending.push(dependent);
                }
            }
        }
        visited
            .into_iter()
            .filter(|key| key.source.is_none())
            .map(|key| key.name.clone())
            .collect()
    }
}

fn package_key(entry: &Table) -> Option<PackageKey> {
    let field = |name: &str| Some(entry.get(name)?.as_str()?.to_string());
    Some(PackageKey {
        name: field("name")?,
        version: field("version")?,
        source: match entry.get("source") {
            None => None,
            Some(_) => Some(field("source")?),
        },
    })
}

/// Resolve a dependency reference, written `name`, `name version`, or
/// `name version (source)` with only as much detail as disambiguation needs.
/// A reference names a Git source without the commit it is locked to, which
/// the package's own `source` carries after `#`. A path package has no source
/// to name, so a reference without one that still matches several packages
/// means the path package among them.
fn resolve_reference<'a>(
    keys_by_name: &BTreeMap<&str, Vec<&'a PackageKey>>,
    reference: &str,
) -> Option<&'a PackageKey> {
    let mut parts = reference.splitn(3, ' ');
    let name = parts.next()?;
    let version = parts.next();
    let source = match parts.next() {
        None => None,
        Some(source) => Some(source.strip_prefix('(')?.strip_suffix(')')?),
    };
    let mut candidates = keys_by_name
        .get(name)?
        .iter()
        .copied()
        .filter(|key| {
            version.is_none_or(|version| key.version == version)
                && source.is_none_or(|source| {
                    key.source.as_deref().map(without_locked_commit)
                        == Some(without_locked_commit(source))
                })
        })
        .collect::<Vec<_>>();
    if candidates.len() > 1 && source.is_none() {
        candidates.retain(|key| key.source.is_none());
    }
    match candidates.as_slice() {
        [candidate] => Some(candidate),
        _ => None,
    }
}

fn without_locked_commit(source: &str) -> &str {
    source.split_once('#').map_or(source, |(source, _)| source)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

    fn path_package(name: &str, dependencies: &[&str]) -> String {
        package_entry(name, "0.1.0", None, dependencies)
    }

    fn registry_package(name: &str, version: &str, checksum: &str, deps: &[&str]) -> String {
        package_entry(name, version, Some(checksum), deps)
    }

    fn package_entry(
        name: &str,
        version: &str,
        checksum: Option<&str>,
        dependencies: &[&str],
    ) -> String {
        let mut entry = format!("[[package]]\nname = \"{name}\"\nversion = \"{version}\"\n");
        if let Some(checksum) = checksum {
            entry.push_str(&format!(
                "source = \"{REGISTRY}\"\nchecksum = \"{checksum}\"\n"
            ));
        }
        if !dependencies.is_empty() {
            entry.push_str("dependencies = [\n");
            for dependency in dependencies {
                entry.push_str(&format!(" \"{dependency}\",\n"));
            }
            entry.push_str("]\n");
        }
        entry
    }

    fn lockfile(version: i64, packages: &[String]) -> String {
        format!("version = {version}\n\n{}", packages.join("\n"))
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(ToString::to_string).collect()
    }

    /// `app` -> `serde`, `top` -> `app`, and an unrelated `cli` member that
    /// depends on `clap`.
    fn workspace(serde_version: &str, serde_checksum: &str) -> String {
        lockfile(
            4,
            &[
                path_package("app", &["serde"]),
                path_package("cli", &["clap"]),
                path_package("top", &["app"]),
                registry_package("clap", "4.0.0", "c1", &[]),
                registry_package("serde", serde_version, serde_checksum, &[]),
            ],
        )
    }

    #[test]
    fn version_bump_reaches_members_whose_entries_are_unchanged() {
        // Dependents name `serde` without a version, so their own entries are
        // identical on both sides; only the graph walk can find them.
        let change = lockfile_change(&workspace("1.0.0", "s1"), &workspace("1.0.1", "s2")).unwrap();

        assert_eq!(change.affected_path_packages, names(&["app", "top"]));
        assert!(change.changed_path_packages.is_empty());
        assert_eq!(
            change.changed_packages,
            names(&["serde 1.0.0", "serde 1.0.1"])
        );
    }

    #[test]
    fn checksum_change_alone_reaches_dependents() {
        // A re-published crate keeps its name, version, and source.
        let change = lockfile_change(&workspace("1.0.0", "s1"), &workspace("1.0.0", "s2")).unwrap();

        assert_eq!(change.affected_path_packages, names(&["app", "top"]));
    }

    #[test]
    fn new_dependency_edge_reaches_the_member_and_its_dependents_only() {
        let base = workspace("1.0.0", "s1");
        let head = base.replace(" \"clap\",\n", " \"clap\",\n \"serde\",\n");
        assert_ne!(base, head);

        let change = lockfile_change(&base, &head).unwrap();

        assert_eq!(change.affected_path_packages, names(&["cli"]));
        assert_eq!(change.changed_path_packages, names(&["cli"]));
    }

    #[test]
    fn change_behind_a_replaced_package_reaches_its_dependents() {
        // `app` depends on the registry `bar`, which `[replace]` swaps for a
        // fork. Only the fork lists `baz`, and only `replace` links the two.
        const FORK: &str = "git+https://example.com/bar#abc";
        let registry_bar = format!("bar 1.0.0 ({REGISTRY})");
        let workspace = |baz_checksum: &str| {
            lockfile(
                4,
                &[
                    path_package("app", &[registry_bar.as_str()]),
                    format!(
                        "[[package]]\nname = \"bar\"\nversion = \"1.0.0\"\nsource = \
                         \"{REGISTRY}\"\nreplace = \"bar 1.0.0 ({FORK})\"\n"
                    ),
                    format!(
                        "[[package]]\nname = \"bar\"\nversion = \"1.0.0\"\nsource = \
                         \"{FORK}\"\ndependencies = [\n \"baz\",\n]\n"
                    ),
                    registry_package("baz", "1.0.0", baz_checksum, &[]),
                ],
            )
        };

        let change = lockfile_change(&workspace("b1"), &workspace("b2")).unwrap();

        assert_eq!(change.affected_path_packages, names(&["app"]));
    }

    #[test]
    fn git_reference_without_the_locked_commit_resolves() {
        // Two `auth` packages force `app` to name a source, and Cargo writes
        // that reference without the commit the package entry is locked to.
        const GIT: &str = "git+https://example.com/auth?branch=main";
        let reference = format!("auth 1.0.0 ({GIT})");
        let workspace = |commit: &str| {
            lockfile(
                4,
                &[
                    path_package("app", &[reference.as_str()]),
                    format!(
                        "[[package]]\nname = \"auth\"\nversion = \"1.0.0\"\nsource = \
                         \"{GIT}#{commit}\"\n"
                    ),
                    registry_package("auth", "1.0.0", "a1", &[]),
                ],
            )
        };

        let change = lockfile_change(&workspace("aaa"), &workspace("bbb")).unwrap();

        assert_eq!(change.affected_path_packages, names(&["app"]));
    }

    #[test]
    fn reference_without_a_source_means_the_path_package_of_that_name() {
        // A registry `foo` shares the local `foo`'s name and version. Cargo
        // qualifies the registry one and leaves the local one bare.
        let registry_foo = format!("foo 0.1.0 ({REGISTRY})");
        let workspace = |checksum: &str| {
            lockfile(
                4,
                &[
                    path_package("app", &["foo 0.1.0"]),
                    path_package("foo", &[]),
                    path_package("other", &[registry_foo.as_str()]),
                    registry_package("foo", "0.1.0", checksum, &[]),
                ],
            )
        };

        let change = lockfile_change(&workspace("f1"), &workspace("f2")).unwrap();

        assert_eq!(change.affected_path_packages, names(&["other"]));
    }

    #[test]
    fn unattributable_lockfiles_are_uncertain() {
        let base = workspace("1.0.0", "s1");
        let ambiguous = lockfile(
            4,
            &[
                path_package("app", &["serde"]),
                registry_package("serde", "1.0.0", "s1", &[]),
                registry_package("serde", "2.0.0", "s2", &[]),
            ],
        );
        let duplicate = lockfile(4, &[path_package("app", &[]), path_package("app", &[])]);
        let pre_v3 = base.replacen("version = 4\n", "", 1);
        type Case = (
            &'static str,
            String,
            String,
            fn(&LockfileUncertainty) -> bool,
        );
        let cases: [Case; 7] = [
            (
                "unparseable head",
                base.clone(),
                "[[package]\n".to_string(),
                |error| matches!(error, LockfileUncertainty::Invalid { side: "head", .. }),
            ),
            (
                "format change",
                base.clone(),
                base.replacen("version = 4", "version = 3", 1),
                |error| matches!(error, LockfileUncertainty::FormatChanged),
            ),
            ("pre-v3 format", pre_v3.clone(), pre_v3, |error| {
                matches!(error, LockfileUncertainty::UnsupportedFormat { .. })
            }),
            ("ambiguous reference", base.clone(), ambiguous, |error| {
                matches!(error, LockfileUncertainty::UnresolvedDependency { .. })
            }),
            (
                "dangling reference",
                base.clone(),
                base.replace(" \"clap\",\n", " \"clap\",\n \"missing\",\n"),
                |error| matches!(error, LockfileUncertainty::UnresolvedDependency { .. }),
            ),
            ("duplicate package", base.clone(), duplicate, |error| {
                matches!(error, LockfileUncertainty::DuplicatePackage { .. })
            }),
            (
                "non-package table",
                base.clone(),
                format!("{base}\n[metadata]\nkey = \"value\"\n"),
                |error| matches!(error, LockfileUncertainty::NonPackageTablesChanged),
            ),
        ];
        for (name, base, head, expected) in cases {
            let error = lockfile_change(&base, &head).expect_err(name);
            assert!(expected(&error), "{name}: unexpected {error:?}");
        }
    }
}
