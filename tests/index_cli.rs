use std::fs;
use std::path::{Path, PathBuf};

use cargo_litmus::affected::{AffectedOptions, InputKind, NextestValidation, compute_affected};
use cargo_litmus::changed_files::ChangedFileInput;
use cargo_litmus::explain::ExplainReport;
use cargo_litmus::ferris::{AffectedCrate, AffectedWorkspace, FerrisAffectedReport, FerrisRunner};
use cargo_litmus::indexer::{
    IndexMode, IndexOptions, build_index, build_or_update_index, defaulted_cache_path,
};
use cargo_litmus::model::EdgeKind;
use git2::Repository;
use tempfile::TempDir;

fn tracked_repo(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    Repository::init(dir.path()).unwrap();

    for (path, contents) in files {
        let full_path = dir.path().join(path);
        fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        fs::write(full_path, contents).unwrap();
    }

    let repo = Repository::open(dir.path()).unwrap();
    let mut index = repo.index().unwrap();
    for (path, _) in files {
        index.add_path(Path::new(path)).unwrap();
    }
    index.write().unwrap();

    dir
}

#[test]
fn indexes_tracked_rust_files_and_writes_default_cache() {
    let repo = tracked_repo(&[
        ("src/lib.rs", "mod foo;\npub fn root() {}\n"),
        ("src/foo.rs", "pub struct Foo;\n"),
    ]);

    let options = IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: None,
    };
    let (metadata, summary) = build_index(&options).unwrap();

    assert_eq!(summary.files_parsed, 2);
    assert!(summary.items_indexed >= 3);
    assert!(Path::new(&summary.cache_path).exists());
    assert!(
        metadata
            .edges
            .iter()
            .any(|edge| edge.kind == EdgeKind::ModuleDeclares)
    );
}

#[test]
fn indexes_cargo_package_targets_and_dependencies() {
    let repo = tracked_repo(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"api\", \"client\", \"helper\"]\nresolver = \"2\"\n",
        ),
        (
            "api/Cargo.toml",
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("api/src/lib.rs", "pub fn root() {}\n"),
        (
            "client/Cargo.toml",
            "[package]\nname = \"client\"\nversion = \"0.1.0\"\nedition = \
             \"2024\"\n\n[dependencies]\napi = { path = \"../api\" \
             }\n\n[dev-dependencies]\nhelper = { path = \"../helper\" }\n",
        ),
        ("client/src/lib.rs", "pub fn client() { api::root(); }\n"),
        ("client/tests/integration.rs", "#[test]\nfn works() {}\n"),
        (
            "helper/Cargo.toml",
            "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("helper/src/lib.rs", "pub fn helper() {}\n"),
    ]);

    let (metadata, _summary) = build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: None,
    })
    .unwrap();

    assert!(
        metadata
            .packages
            .iter()
            .any(|package| package.name == "api")
    );
    assert!(
        metadata
            .targets
            .iter()
            .any(|target| target.name == "integration" && target.is_test())
    );
    let mut dependency_kinds = metadata
        .package_dependencies
        .iter()
        .map(|dependency| dependency.kind.as_str())
        .collect::<Vec<_>>();
    dependency_kinds.sort();
    assert_eq!(dependency_kinds, vec!["dev", "normal"]);
    let mut dependency_paths = metadata
        .package_dependencies
        .iter()
        .map(|dependency| {
            (
                dependency.name.as_str(),
                dependency.depends_on_path.as_str(),
            )
        })
        .collect::<Vec<_>>();
    dependency_paths.sort();
    assert_eq!(dependency_paths, vec![("api", "api"), ("helper", "helper")]);
    assert!(
        metadata
            .package_dependencies
            .iter()
            .all(|dependency| dependency.target.is_none())
    );
    let explanation = ExplainReport::for_package(&metadata, "api").unwrap();
    assert!(explanation.selected_packages.contains(&"api".to_string()));
    assert!(
        explanation
            .selected_packages
            .contains(&"client".to_string())
    );
    assert!(explanation.selection_chains.iter().any(|chain| {
        chain.from == "api"
            && chain.to == "client"
            && chain.relationship == "reverse Cargo dependency"
    }));
}

#[test]
fn indexes_cross_workspace_path_dependencies() {
    let repo = tracked_repo(&[
        (
            "core/Cargo.toml",
            "[workspace]\nmembers = [\"math\"]\nresolver = \"2\"\n",
        ),
        (
            "core/math/Cargo.toml",
            "[package]\nname = \"math\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("core/math/src/lib.rs", "pub fn math() {}\n"),
        (
            "sdk/Cargo.toml",
            "[workspace]\nmembers = [\"state\"]\nresolver = \"2\"\n",
        ),
        (
            "sdk/state/Cargo.toml",
            "[package]\nname = \"state\"\nversion = \"0.1.0\"\nedition = \
             \"2024\"\n\n[dependencies]\nmath = { path = \"../../core/math\" }\n",
        ),
        ("sdk/state/src/lib.rs", "pub fn state() { math::math(); }\n"),
        (
            "nodes/Cargo.toml",
            "[workspace]\nmembers = [\"api\"]\nresolver = \"2\"\n",
        ),
        (
            "nodes/api/Cargo.toml",
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \
             \"2024\"\n\n[dependencies]\nstate = { path = \"../../sdk/state\" }\n",
        ),
        ("nodes/api/src/lib.rs", "pub fn api() { state::state(); }\n"),
    ]);

    let (metadata, _summary) = build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: None,
    })
    .unwrap();

    let package_workspace = |name: &str| {
        metadata
            .packages
            .iter()
            .find(|package| package.name == name)
            .map(|package| package.workspace.as_str())
            .unwrap()
            .to_string()
    };
    assert_eq!(package_workspace("math"), "core");
    assert_eq!(package_workspace("state"), "sdk");
    assert_eq!(package_workspace("api"), "nodes");

    let package_id = |name: &str| {
        metadata
            .packages
            .iter()
            .find(|package| package.name == name)
            .map(|package| package.id.clone())
            .unwrap()
    };
    let math = package_id("math");
    let state = package_id("state");
    let api = package_id("api");
    assert!(metadata.package_dependencies.iter().any(|dependency| {
        dependency.package_id == state && dependency.depends_on_package_id == math
    }));
    assert!(metadata.package_dependencies.iter().any(|dependency| {
        dependency.package_id == api && dependency.depends_on_package_id == state
    }));
}

#[test]
fn cargo_metadata_failure_fails_index_build() {
    let repo = tracked_repo(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"missing\"]\nresolver = \"2\"\n",
        ),
        ("src/lib.rs", "pub fn root() {}\n"),
    ]);

    let err = build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("failed to load Cargo metadata"));
}

#[test]
fn missing_tracked_file_fails_by_default() {
    let repo = tracked_repo(&[("src/lib.rs", "pub fn root() {}\n")]);
    fs::remove_file(repo.path().join("src/lib.rs")).unwrap();

    let err = build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("indexing failed"));
}

#[test]
fn malformed_rust_file_fails_by_default() {
    let repo = tracked_repo(&[("src/lib.rs", "pub fn nope( {\n")]);

    let err = build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: None,
    })
    .unwrap_err();

    assert!(err.to_string().contains("failed to parse Rust source"));
}

#[test]
fn default_cache_path_is_under_selected_root() {
    let path = defaulted_cache_path(Path::new("/tmp/example"));
    assert_eq!(path, expected_default_cache_path(Path::new("/tmp/example")));
}

fn expected_default_cache_path(root: &Path) -> PathBuf {
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("target"));
    let target_dir = if target_dir.is_absolute() {
        target_dir
    } else {
        root.join(target_dir)
    };
    target_dir.join("cargo-litmus.rkyv")
}

#[derive(Default)]
struct MockFerrisRunner {
    ripples_fails: bool,
}

impl FerrisRunner for MockFerrisRunner {
    fn ripples(
        &self,
        _root: &Path,
        _files: &[String],
    ) -> cargo_litmus::Result<FerrisAffectedReport> {
        if self.ripples_fails {
            return Err(cargo_litmus::LitmusError::ParseExternalJson {
                source_name: "mock".to_string(),
                message: "ripples failed".to_string(),
            });
        }

        Ok(FerrisAffectedReport {
            affected_crates: vec![
                AffectedCrate {
                    name: "direct".to_string(),
                    workspace: "workspace-a".to_string(),
                    is_directly_affected: true,
                    is_standalone: false,
                },
                AffectedCrate {
                    name: "dependent".to_string(),
                    workspace: "workspace-a".to_string(),
                    is_directly_affected: false,
                    is_standalone: false,
                },
            ],
            affected_workspaces: vec![AffectedWorkspace {
                name: "workspace-a".to_string(),
                path: "workspace-a".to_string(),
            }],
            directly_affected_crates: vec!["direct".to_string()],
            directly_affected_workspaces: vec![AffectedWorkspace {
                name: "workspace-a".to_string(),
                path: "workspace-a".to_string(),
            }],
        })
    }

    fn lineup(&self, _root: &Path) -> cargo_litmus::Result<FerrisAffectedReport> {
        Ok(FerrisAffectedReport {
            affected_crates: Vec::new(),
            affected_workspaces: vec![
                AffectedWorkspace {
                    name: "workspace-a".to_string(),
                    path: "workspace-a".to_string(),
                },
                AffectedWorkspace {
                    name: "workspace-b".to_string(),
                    path: "workspace-b".to_string(),
                },
            ],
            directly_affected_crates: Vec::new(),
            directly_affected_workspaces: vec![
                AffectedWorkspace {
                    name: "workspace-a".to_string(),
                    path: "workspace-a".to_string(),
                },
                AffectedWorkspace {
                    name: "workspace-b".to_string(),
                    path: "workspace-b".to_string(),
                },
            ],
        })
    }
}

struct MultiWorkspaceFerrisRunner;

impl FerrisRunner for MultiWorkspaceFerrisRunner {
    fn ripples(
        &self,
        _root: &Path,
        _files: &[String],
    ) -> cargo_litmus::Result<FerrisAffectedReport> {
        Ok(FerrisAffectedReport {
            affected_crates: vec![
                AffectedCrate {
                    name: "cargo-litmus".to_string(),
                    workspace: "cargo-litmus".to_string(),
                    is_directly_affected: true,
                    is_standalone: true,
                },
                AffectedCrate {
                    name: "service-api".to_string(),
                    workspace: "services".to_string(),
                    is_directly_affected: true,
                    is_standalone: false,
                },
                AffectedCrate {
                    name: "service-worker".to_string(),
                    workspace: "services".to_string(),
                    is_directly_affected: true,
                    is_standalone: false,
                },
            ],
            affected_workspaces: vec![
                AffectedWorkspace {
                    name: "cargo-litmus".to_string(),
                    path: "sandbox/cargo-litmus".to_string(),
                },
                AffectedWorkspace {
                    name: "services".to_string(),
                    path: "services".to_string(),
                },
            ],
            directly_affected_crates: vec![
                "cargo-litmus".to_string(),
                "service-api".to_string(),
                "service-worker".to_string(),
            ],
            directly_affected_workspaces: vec![
                AffectedWorkspace {
                    name: "cargo-litmus".to_string(),
                    path: "sandbox/cargo-litmus".to_string(),
                },
                AffectedWorkspace {
                    name: "services".to_string(),
                    path: "services".to_string(),
                },
            ],
        })
    }

    fn lineup(&self, _root: &Path) -> cargo_litmus::Result<FerrisAffectedReport> {
        Ok(FerrisAffectedReport {
            affected_crates: Vec::new(),
            affected_workspaces: vec![
                AffectedWorkspace {
                    name: "cargo-litmus".to_string(),
                    path: "sandbox/cargo-litmus".to_string(),
                },
                AffectedWorkspace {
                    name: "services".to_string(),
                    path: "services".to_string(),
                },
            ],
            directly_affected_crates: Vec::new(),
            directly_affected_workspaces: vec![
                AffectedWorkspace {
                    name: "cargo-litmus".to_string(),
                    path: "sandbox/cargo-litmus".to_string(),
                },
                AffectedWorkspace {
                    name: "services".to_string(),
                    path: "services".to_string(),
                },
            ],
        })
    }
}

#[test]
fn affected_uses_ferris_ripples_for_package_selection() {
    let repo = tracked_repo(&[("src/lib.rs", "pub fn root() {}\n")]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["src/lib.rs".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MockFerrisRunner::default(),
    )
    .unwrap();

    assert!(!report.failed_wide);
    assert_eq!(report.directly_affected_crates, vec!["direct"]);
    assert_eq!(report.affected_crates.len(), 2);
}

#[test]
fn affected_uses_warm_base_cache_for_file_diagnostics() {
    let repo = tracked_repo(&[("src/lib.rs", "pub fn root() {}\n")]);
    let base_cache = repo.path().join("target/base.rkyv");
    let current_cache = repo.path().join("target/current.rkyv");
    let (_base, _summary) = build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: Some(base_cache.clone()),
    })
    .unwrap();
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn root() {}\n// comment\n",
    )
    .unwrap();

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["src/lib.rs".to_string()],
            changed_files: Vec::new(),
            base_cache: Some(base_cache),
            current_cache: Some(current_cache.clone()),
        },
        &MockFerrisRunner::default(),
    )
    .unwrap();

    assert_eq!(
        report.cache_status,
        cargo_litmus::affected::CacheStatus::Warm
    );
    assert_eq!(
        report.changed_files[0].status,
        cargo_litmus::affected::FileChangeStatus::UnchangedStructureModified
    );
    assert!(current_cache.exists());
}

#[test]
fn incremental_index_reuses_unchanged_files_and_reparses_changed_files() {
    let repo = tracked_repo(&[
        ("src/lib.rs", "mod foo;\npub fn root() {}\n"),
        ("src/foo.rs", "pub fn foo() {}\n"),
    ]);
    let cache = repo.path().join("target/current.rkyv");
    build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: Some(cache.clone()),
    })
    .unwrap();
    fs::write(
        repo.path().join("src/foo.rs"),
        "pub fn foo() {}\n// changed\n",
    )
    .unwrap();

    let (_metadata, summary) = build_or_update_index(
        &IndexOptions {
            root: repo.path().to_path_buf(),
            cache_path: Some(cache),
        },
        &[ChangedFileInput::path_only("src/foo.rs")],
    )
    .unwrap();

    assert_eq!(summary.index_mode, IndexMode::IncrementalUpdate);
    assert_eq!(summary.files_reused, 1);
    assert_eq!(summary.files_reparsed, 1);
}

#[test]
fn incremental_index_treats_declared_missing_file_as_deletion() {
    let repo = tracked_repo(&[
        ("src/lib.rs", "mod removed;\n"),
        ("src/removed.rs", "pub fn removed() {}\n"),
    ]);
    let cache = repo.path().join("target/current.rkyv");
    build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: Some(cache.clone()),
    })
    .unwrap();
    fs::remove_file(repo.path().join("src/removed.rs")).unwrap();

    let (metadata, summary) = build_or_update_index(
        &IndexOptions {
            root: repo.path().to_path_buf(),
            cache_path: Some(cache),
        },
        &[ChangedFileInput::path_only("src/removed.rs")],
    )
    .unwrap();

    assert_eq!(summary.files_removed, 1);
    assert!(
        !metadata
            .files
            .iter()
            .any(|file| file.path == "src/removed.rs")
    );
}

#[test]
fn incremental_index_refreshes_cargo_metadata_when_manifest_changes() {
    let repo = tracked_repo(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"api\"]\nresolver = \"2\"\n",
        ),
        (
            "api/Cargo.toml",
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("api/src/lib.rs", "pub fn api() {}\n"),
    ]);
    let cache = repo.path().join("target/current.rkyv");
    build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: Some(cache.clone()),
    })
    .unwrap();
    fs::write(
        repo.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"missing\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(repo.path().join("api/src/lib.rs"), "pub fn api_v2() {}\n").unwrap();

    let err = build_or_update_index(
        &IndexOptions {
            root: repo.path().to_path_buf(),
            cache_path: Some(cache),
        },
        &[
            ChangedFileInput::path_only("Cargo.toml"),
            ChangedFileInput::path_only("api/src/lib.rs"),
        ],
    )
    .unwrap_err();

    assert!(err.to_string().contains("failed to load Cargo metadata"));
}

#[test]
fn incremental_index_trusts_unchanged_file_list_for_unlisted_files() {
    let repo = tracked_repo(&[
        ("src/lib.rs", "mod foo;\npub fn root() {}\n"),
        ("src/foo.rs", "pub fn foo() {}\n"),
    ]);
    let cache = repo.path().join("target/current.rkyv");
    build_index(&IndexOptions {
        root: repo.path().to_path_buf(),
        cache_path: Some(cache.clone()),
    })
    .unwrap();
    fs::write(
        repo.path().join("src/foo.rs"),
        "pub fn foo() {}\n// changed outside changed-file list\n",
    )
    .unwrap();

    let (_metadata, summary) = build_or_update_index(
        &IndexOptions {
            root: repo.path().to_path_buf(),
            cache_path: Some(cache),
        },
        &[ChangedFileInput::path_only("src/lib.rs")],
    )
    .unwrap();

    assert_eq!(summary.index_mode, IndexMode::WarmUnmodified);
    assert_eq!(summary.files_reused, 2);
    assert_eq!(summary.files_reparsed, 0);
}

#[test]
fn arbitrary_package_root_input_selects_package_without_uncertainty() {
    let repo = tracked_repo(&[
        ("services/src/lib.rs", "pub fn api() {}\n"),
        ("sandbox/cargo-litmus/src/lib.rs", "pub fn litmus() {}\n"),
        (
            "sandbox/cargo-litmus/Cargo.toml",
            "[package]\nname = \"cargo-litmus\"\n",
        ),
        ("sandbox/cargo-litmus/README.md", "litmus notes\n"),
    ]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec![
                "services/src/lib.rs".to_string(),
                "sandbox/cargo-litmus/README.md".to_string(),
            ],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MultiWorkspaceFerrisRunner,
    )
    .unwrap();

    assert!(!report.failed_wide);
    assert!(report.unknown_files.is_empty());
    assert_eq!(report.nextest_validation, NextestValidation::Skipped);
    assert!(report.input_explanations.iter().any(|input| {
        input.path == "sandbox/cargo-litmus/README.md"
            && input.kind == InputKind::PackageInput
            && !input.failed_wide
    }));
    let litmus = report
        .workspace_selections
        .iter()
        .find(|selection| selection.workspace.name == "cargo-litmus")
        .unwrap();
    assert!(!litmus.failed_wide);
    assert!(litmus.unknown_files.is_empty());
    let services = report
        .workspace_selections
        .iter()
        .find(|selection| selection.workspace.name == "services")
        .unwrap();
    assert!(!services.failed_wide);
    assert!(services.unknown_files.is_empty());
}

#[test]
fn included_repository_input_maps_back_to_owning_package() {
    let repo = tracked_repo(&[
        (
            "sandbox/cargo-litmus/Cargo.toml",
            "[package]\nname = \"cargo-litmus\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        (
            "sandbox/cargo-litmus/src/lib.rs",
            "const SCHEMA: &str = include_str!(\"../../../schemas/api.json\");\n",
        ),
        ("schemas/api.json", "{}\n"),
    ]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["schemas/api.json".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MultiWorkspaceFerrisRunner,
    )
    .unwrap();

    assert!(report.unknown_files.is_empty());
    assert!(report.input_explanations.iter().any(|input| {
        input.path == "schemas/api.json"
            && input.kind == InputKind::IncludedSource
            && input.packages.contains(&"cargo-litmus".to_string())
    }));
}

#[test]
fn indexed_included_rust_source_selects_consuming_package() {
    let repo = tracked_repo(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"producer\", \"consumer\"]\nresolver = \"2\"\n",
        ),
        (
            "producer/Cargo.toml",
            "[package]\nname = \"producer\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("producer/src/lib.rs", "pub fn producer() {}\n"),
        ("producer/src/generated.rs", "pub fn generated() {}\n"),
        (
            "consumer/Cargo.toml",
            "[package]\nname = \"consumer\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        (
            "consumer/src/lib.rs",
            "include!(\"../../producer/src/generated.rs\");\n",
        ),
    ]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["producer/src/generated.rs".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MockFerrisRunner::default(),
    )
    .unwrap();

    let input = report
        .input_explanations
        .iter()
        .find(|input| input.path == "producer/src/generated.rs")
        .unwrap();
    assert_eq!(input.kind, InputKind::IncludedSource);
    assert!(input.direct_packages.contains(&"producer".to_string()));
    assert!(input.direct_packages.contains(&"consumer".to_string()));
    assert!(
        report
            .affected_crates
            .iter()
            .any(|krate| krate.name == "consumer")
    );
}

#[test]
fn cargo_config_is_reported_distinctly_and_fails_wide() {
    let repo = tracked_repo(&[
        ("services/src/lib.rs", "pub fn api() {}\n"),
        (".cargo/config.toml", "[build]\nrustflags = []\n"),
    ]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec![".cargo/config.toml".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MultiWorkspaceFerrisRunner,
    )
    .unwrap();

    assert!(report.failed_wide);
    assert!(
        report
            .input_explanations
            .iter()
            .any(|input| { input.kind == InputKind::CargoConfig && input.failed_wide })
    );
}

#[test]
fn rust_toolchain_is_reported_distinctly_and_fails_wide() {
    let repo = tracked_repo(&[
        (
            "Cargo.toml",
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \
             \"2024\"\n\n[workspace]\nmembers = [\"sibling\"]\nresolver = \"2\"\n",
        ),
        ("src/lib.rs", "pub fn root() {}\n"),
        (
            "sibling/Cargo.toml",
            "[package]\nname = \"sibling\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("sibling/src/lib.rs", "pub fn sibling() {}\n"),
        ("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n"),
    ]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["rust-toolchain.toml".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MultiWorkspaceFerrisRunner,
    )
    .unwrap();

    assert!(report.failed_wide);
    assert!(report.input_explanations.iter().any(|input| {
        input.path == "rust-toolchain.toml"
            && input.kind == InputKind::RustToolchain
            && input.failed_wide
    }));
}

#[test]
fn portable_cache_reuses_verified_files_and_refreshes_identity() {
    let files = [
        ("src/lib.rs", "mod foo;\n"),
        ("src/foo.rs", "pub fn foo() {}\n"),
    ];
    let first = tracked_repo(&files);
    let second = tracked_repo(&files);
    let first_cache = first.path().join("target/cargo-litmus.rkyv");
    let second_cache = second.path().join("target/cargo-litmus.rkyv");
    build_index(&IndexOptions {
        root: first.path().to_path_buf(),
        cache_path: Some(first_cache.clone()),
    })
    .unwrap();
    fs::create_dir_all(second_cache.parent().unwrap()).unwrap();
    fs::copy(first_cache, &second_cache).unwrap();

    let (metadata, summary) = build_or_update_index(
        &IndexOptions {
            root: second.path().to_path_buf(),
            cache_path: Some(second_cache),
        },
        &[],
    )
    .unwrap();

    assert_eq!(summary.index_mode, IndexMode::IncrementalUpdate);
    assert_eq!(summary.files_reused, 2);
    assert_eq!(summary.files_reparsed, 0);
    assert_eq!(
        Path::new(&metadata.repo_root_hint).canonicalize().unwrap(),
        second.path().canonicalize().unwrap()
    );
}

#[test]
fn configured_non_rust_input_selects_named_package_without_uncertainty() {
    let repo = tracked_repo(&[
        (
            "Cargo.toml",
            "[package]\nname = \"server\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("src/lib.rs", "pub fn server() {}\n"),
        ("schemas/api.json", "{}\n"),
        (
            ".cargo-litmus.toml",
            "[[rules]]\npaths = [\"schemas/**\"]\npackages = [\"server\"]\nselection = \
             \"packages\"\n",
        ),
    ]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["schemas/api.json".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MockFerrisRunner::default(),
    )
    .unwrap();

    assert_eq!(report.configured_inputs.len(), 1);
    assert!(report.unknown_files.is_empty());
    assert!(
        report
            .affected_crates
            .iter()
            .any(|krate| krate.name == "server")
    );
}

#[test]
fn configured_package_input_selects_reverse_dependents() {
    let repo = tracked_repo(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"server\", \"client\"]\nresolver = \"2\"\n",
        ),
        (
            "server/Cargo.toml",
            "[package]\nname = \"server\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("server/src/lib.rs", "pub fn server() {}\n"),
        (
            "client/Cargo.toml",
            "[package]\nname = \"client\"\nversion = \"0.1.0\"\nedition = \
             \"2024\"\n[dependencies]\nserver = { path = \"../server\" }\n",
        ),
        (
            "client/src/lib.rs",
            "pub fn client() { server::server(); }\n",
        ),
        ("schemas/api.json", "{}\n"),
        (
            ".cargo-litmus.toml",
            "[[rules]]\npaths = [\"schemas/**\"]\npackages = [\"server\"]\nselection = \
             \"packages\"\n",
        ),
    ]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["schemas/api.json".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MockFerrisRunner::default(),
    )
    .unwrap();

    assert!(
        report
            .affected_crates
            .iter()
            .any(|krate| krate.name == "server")
    );
    assert!(
        report
            .affected_crates
            .iter()
            .any(|krate| krate.name == "client")
    );
    let input = &report.input_explanations[0];
    assert!(input.direct_packages.contains(&"server".to_string()));
    assert!(
        input
            .dependency_chains
            .iter()
            .any(|chain| { chain.from_package == "server" && chain.to_package == "client" })
    );
}

#[test]
fn configured_input_is_not_dropped_by_mixed_test_only_selection() {
    let repo = tracked_repo(&[
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"server\", \"client\"]\nresolver = \"2\"\n",
        ),
        (
            "server/Cargo.toml",
            "[package]\nname = \"server\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("server/src/lib.rs", "pub fn server() {}\n"),
        (
            "client/Cargo.toml",
            "[package]\nname = \"client\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("client/src/lib.rs", "pub fn client() {}\n"),
        ("client/tests/integration.rs", "#[test]\nfn works() {}\n"),
        ("schemas/api.json", "{}\n"),
        (
            ".cargo-litmus.toml",
            "[[rules]]\npaths = [\"schemas/**\"]\npackages = [\"server\"]\nselection = \
             \"packages\"\n",
        ),
    ]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec![
                "schemas/api.json".to_string(),
                "client/tests/integration.rs".to_string(),
            ],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MockFerrisRunner::default(),
    )
    .unwrap();

    assert!(
        report
            .affected_test_crates
            .iter()
            .any(|krate| krate.name == "server")
    );
    assert!(
        report
            .affected_test_crates
            .iter()
            .any(|krate| krate.name == "client")
    );
}

#[test]
fn generated_output_without_static_relationship_remains_uncertain() {
    let repo = tracked_repo(&[
        (
            "sandbox/cargo-litmus/Cargo.toml",
            "[package]\nname = \"cargo-litmus\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
        ("sandbox/cargo-litmus/src/lib.rs", "pub fn litmus() {}\n"),
    ]);
    let generated = "sandbox/cargo-litmus/target/out/generated.rs";

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec![generated.to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MultiWorkspaceFerrisRunner,
    )
    .unwrap();

    assert!(report.unknown_files.contains(&generated.to_string()));
    assert!(report.input_explanations.iter().any(|input| {
        input.path == generated && input.kind == InputKind::Uncertain && input.failed_wide
    }));
}

#[test]
fn affected_unknown_file_outside_affected_workspaces_fails_globally() {
    let repo = tracked_repo(&[("services/src/lib.rs", "pub fn api() {}\n")]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["outside/Cargo.toml".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MultiWorkspaceFerrisRunner,
    )
    .unwrap();

    assert!(report.failed_wide);
    assert_eq!(report.affected_workspaces.len(), 2);
    assert!(
        report
            .workspace_selections
            .iter()
            .all(|selection| selection.failed_wide)
    );
}

#[test]
fn affected_missing_file_fails_wide() {
    let repo = tracked_repo(&[("src/lib.rs", "pub fn root() {}\n")]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["src/missing.rs".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MockFerrisRunner::default(),
    )
    .unwrap();

    assert!(report.failed_wide);
    assert_eq!(report.unknown_files, vec!["src/missing.rs"]);
    assert_eq!(report.affected_workspaces.len(), 2);
}

#[test]
fn affected_malformed_current_source_fails_wide() {
    let repo = tracked_repo(&[("src/lib.rs", "pub fn nope( {\n")]);

    let report = compute_affected(
        &AffectedOptions {
            root: repo.path().to_path_buf(),
            files: vec!["src/lib.rs".to_string()],
            changed_files: Vec::new(),
            base_cache: None,
            current_cache: None,
        },
        &MockFerrisRunner::default(),
    )
    .unwrap();

    assert!(report.failed_wide);
    assert_eq!(report.affected_workspaces.len(), 2);
}
