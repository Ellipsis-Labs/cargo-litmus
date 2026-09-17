//! End-to-end accuracy scenarios for cargo-litmus.
//!
//! Each scenario synthesizes a multi-workspace monorepo, builds a base index,
//! commits a change, and runs the real `cargo-litmus affected` binary against
//! it. Expectations are two-sided:
//!
//! - `required`: packages whose tests MUST be selected. A violation is a false
//!   negative and always a bug.
//! - `allowed`: packages that MAY be selected. A violation is over-selection
//!   beyond the scenario's declared conservative allowance.
//!
//! Scenarios using the default scripted ferris-wheel report the same
//! reverse-transitive dependency closure upstream ferris-wheel computes, so
//! failures point at litmus's own narrowing and widening logic.

use std::fs;

use crate::support::{
    CommandExpectation, CommandMode, Expect, FerrisCrate, FerrisPayload, FerrisWorkspace, Monorepo,
    check, check_with_ferris, check_with_wrapped_cargo, create, delete, edit, pkg, rename, target,
    ws,
};

#[test]
fn leaf_change_selects_only_owning_package() {
    let monorepo = Monorepo::new(vec![ws("core").package(pkg("math"))]);
    check(
        "leaf_change_selects_only_owning_package",
        monorepo,
        &[edit("core/math/src/lib.rs", "pub fn math() -> u32 { 2 }\n")],
        &Expect::exact(&[target("core/math")]).note("no dependents exist"),
    );
}

#[test]
fn dependent_in_same_workspace_is_selected() {
    let monorepo = Monorepo::new(vec![
        ws("core")
            .package(pkg("math"))
            .package(pkg("app").dep("math")),
    ]);
    check(
        "dependent_in_same_workspace_is_selected",
        monorepo,
        &[edit("core/math/src/lib.rs", "pub fn math() -> u32 { 2 }\n")],
        &Expect::exact(&[target("core/math"), target("core/app")]),
    );
}

#[test]
fn cross_workspace_dependents_do_not_pull_in_unrelated_workspaces() {
    let monorepo = Monorepo::new(vec![
        ws("core").package(pkg("math")),
        ws("sdk")
            .package(pkg("state").dep_at("math", "../../core/math"))
            .package(pkg("unrelated")),
        ws("nodes").package(
            pkg("api")
                .dep_at("state", "../../sdk/state")
                .dep_at("unrelated", "../../sdk/unrelated"),
        ),
    ]);
    check(
        "cross_workspace_dependents_do_not_pull_in_unrelated_workspaces",
        monorepo,
        &[edit("core/math/src/lib.rs", "pub fn math() -> u32 { 2 }\n")],
        &Expect::exact(&[
            target("core/math"),
            target("sdk/state"),
            target("nodes/api"),
        ])
        .note("sdk/unrelated is not on the dependency path and must stay unselected"),
    );
}

#[test]
fn dev_dependency_holder_is_selected_but_nothing_extra() {
    let monorepo = Monorepo::new(vec![
        ws("core").package(pkg("leaf")),
        ws("app")
            .package(pkg("consumer").dev_dep_at("leaf", "../../core/leaf"))
            .package(pkg("top").dep("consumer")),
    ]);
    check(
        "dev_dependency_holder_is_selected_but_nothing_extra",
        monorepo,
        &[edit("core/leaf/src/lib.rs", "pub fn leaf() -> u32 { 2 }\n")],
        &Expect::exact(&[
            target("core/leaf"),
            target("app/consumer"),
            target("app/top"),
        ])
        .note("scripted ferris reports dev-dependent holders and their dependents"),
    );
}

#[test]
fn build_dependency_change_selects_builder_and_its_dependents() {
    let monorepo = Monorepo::new(vec![
        ws("tools").package(pkg("codegen")),
        ws("generated")
            .package(pkg("gen").build_dep_at("codegen", "../../tools/codegen"))
            .package(pkg("downstream").dep("gen")),
    ]);
    check(
        "build_dependency_change_selects_builder_and_its_dependents",
        monorepo,
        &[edit(
            "tools/codegen/src/lib.rs",
            "pub fn codegen() -> u32 { 2 }\n",
        )],
        &Expect::exact(&[
            target("tools/codegen"),
            target("generated/gen"),
            target("generated/downstream"),
        ]),
    );
}

#[test]
fn optional_feature_activated_dependency_closure_is_selected() {
    let monorepo = Monorepo::new(vec![
        ws("lib")
            .package(pkg("leaf"))
            .package(
                pkg("holder")
                    .optional_dep("leaf")
                    .feature("leafy", &["dep:leaf"]),
            )
            .package(pkg("user").dep_with_features("holder", &["leafy"])),
    ]);
    check(
        "optional_feature_activated_dependency_closure_is_selected",
        monorepo,
        &[edit("lib/leaf/src/lib.rs", "pub fn leaf() -> u32 { 2 }\n")],
        &Expect::exact(&[target("lib/leaf"), target("lib/holder"), target("lib/user")])
            .note("feature activation keeps the optional edge live"),
    );
}

#[test]
fn test_module_change_narrows_to_module_filter() {
    let base_lib = "pub fn adds(left: u32, right: u32) -> u32 {\n    left + \
                    right\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    \
                    fn adds_numbers() {\n        assert_eq!(adds(1, 2), 3);\n    }\n}\n";
    let changed_lib =
        "pub fn adds(left: u32, right: u32) -> u32 {\n    left + right\n}\n\n#[cfg(test)]\nmod \
         tests {\n    use super::*;\n\n    #[test]\n    fn adds_numbers() {\n        \
         assert_eq!(adds(1, 2), 3);\n    }\n\n    #[test]\n    fn adds_zero() {\n        \
         assert_eq!(adds(0, 0), 0);\n    }\n}\n";
    let monorepo = Monorepo::new(vec![
        ws("core")
            .package(pkg("math").lib_source(base_lib))
            .package(pkg("app").dep("math")),
    ]);
    check(
        "test_module_change_narrows_to_module_filter",
        monorepo,
        &[edit("core/math/src/lib.rs", changed_lib)],
        &Expect::exact(&[target("core/math")])
            .require_command(CommandExpectation::new("core", CommandMode::Module).package("math"))
            .note("a change inside #[cfg(test)] cannot affect dependents"),
    );
}

#[test]
fn integration_test_file_change_selects_only_owning_test_target() {
    let monorepo = Monorepo::new(vec![
        ws("app").package(pkg("helper")).package(
            pkg("consumer")
                .dev_dep("helper")
                .file("tests/integration.rs", "#[test]\nfn works() {}\n"),
        ),
    ]);
    check(
        "integration_test_file_change_selects_only_owning_test_target",
        monorepo,
        &[edit(
            "app/consumer/tests/integration.rs",
            "#[test]\nfn works() {}\n\n#[test]\nfn also_works() {}\n",
        )],
        &Expect::exact(&[target("app/consumer")]).require_command(
            CommandExpectation::new("app", CommandMode::TestTarget).package("consumer"),
        ),
    );
}

#[test]
fn inert_manifest_change_in_leaf_stays_package_local() {
    let monorepo = Monorepo::new(vec![ws("core").package(pkg("math"))]);
    check(
        "inert_manifest_change_in_leaf_stays_package_local",
        monorepo,
        &[edit(
            "core/math/Cargo.toml",
            "[package]\nname = \"math\"\nversion = \"0.1.0\"\nedition = \"2021\"\ndescription = \
             \"math helpers\"\n",
        )],
        &Expect::exact(&[target("core/math")])
            .note("an inert manifest edit selects the owning package, not the workspace"),
    );
}

#[test]
fn package_data_file_change_stays_within_dependency_closure() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("api").file("fixtures/data.json", "{\"version\": 1}\n"))
            .package(pkg("client").dep("api")),
    ]);
    check(
        "package_data_file_change_stays_within_dependency_closure",
        monorepo,
        &[edit("app/api/fixtures/data.json", "{\"version\": 2}\n")],
        &Expect::exact(&[target("app/api"), target("app/client")])
            .note("package inputs map to their owning package without workspace-wide uncertainty"),
    );
}

#[test]
fn untouched_workspaces_stay_unselected() {
    let monorepo = Monorepo::new(vec![
        ws("alpha").package(pkg("alpha-core")),
        ws("beta").package(pkg("beta-core")),
        ws("gamma").package(pkg("gamma-core")),
    ]);
    check(
        "untouched_workspaces_stay_unselected",
        monorepo,
        &[edit(
            "alpha/alpha-core/src/lib.rs",
            "pub fn alpha_core() -> u32 { 2 }\n",
        )],
        &Expect::exact(&[target("alpha/alpha-core")]).forbid_global_fail_wide(),
    );
}

#[test]
fn renamed_source_file_keeps_selection_narrow() {
    let monorepo = Monorepo::new(vec![
        ws("core").package(
            pkg("math")
                .lib_source("mod extra;\n\npub fn adds() -> u32 { 1 }\n")
                .file("src/extra.rs", "pub fn extra() -> u32 { 2 }\n"),
        ),
    ]);
    check(
        "renamed_source_file_keeps_selection_narrow",
        monorepo,
        &[
            rename("core/math/src/extra.rs", "core/math/src/renamed.rs"),
            edit(
                "core/math/src/lib.rs",
                "mod renamed;\n\npub fn adds() -> u32 { 1 }\n",
            ),
        ],
        &Expect::exact(&[target("core/math")]),
    );
}

#[test]
fn deleted_source_file_keeps_selection_narrow() {
    let monorepo = Monorepo::new(vec![
        ws("core").package(
            pkg("math")
                .lib_source("mod extra;\n\npub fn adds() -> u32 { 1 }\n")
                .file("src/extra.rs", "pub fn extra() -> u32 { 2 }\n"),
        ),
    ]);
    check(
        "deleted_source_file_keeps_selection_narrow",
        monorepo,
        &[
            delete("core/math/src/extra.rs"),
            edit("core/math/src/lib.rs", "pub fn adds() -> u32 { 1 }\n"),
        ],
        &Expect::exact(&[target("core/math")]),
    );
}

#[test]
fn generated_input_widens_only_its_own_workspace() {
    let monorepo = Monorepo::new(vec![
        ws("core").package(pkg("math")),
        ws("sdk").package(pkg("state").dep_at("math", "../../core/math")),
    ]);
    check(
        "generated_input_widens_only_its_own_workspace",
        monorepo,
        &[create(
            "core/math/target/generated.rs",
            "pub fn generated() {}\n",
        )],
        &Expect::exact(&[target("core/math"), target("sdk/state")])
            .require_command(CommandExpectation::new("core", CommandMode::WorkspaceWide))
            .forbid_global_fail_wide()
            .note("generated inputs widen the owning workspace only"),
    );
}

#[test]
fn unmapped_file_outside_workspaces_stays_conservative() {
    let monorepo = Monorepo::new(vec![
        ws("core").package(pkg("math")),
        ws("sdk").package(pkg("state").dep_at("math", "../../core/math")),
    ]);
    check(
        "unmapped_file_outside_workspaces_stays_conservative",
        monorepo,
        &[create("docs/notes.md", "release notes\n")],
        &Expect::exact(&[target("core/math"), target("sdk/state")]).note(
            "a file outside every workspace has no provable owner; widening to all workspaces is \
             the sanctioned conservative fallback (false negatives are never acceptable)",
        ),
    );
}

#[test]
fn mixed_test_only_change_does_not_pull_in_its_dependents() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("shared"))
            .package(pkg("api").dep("shared"))
            .package(pkg("reporter").dep("shared")),
    ]);
    check(
        "mixed_test_only_change_does_not_pull_in_its_dependents",
        monorepo,
        &[
            edit("app/api/src/lib.rs", "pub fn api() -> u32 { 2 }\n"),
            edit(
                "app/shared/tests/smoke.rs",
                "#[test]\nfn smoke() {}\n\n#[test]\nfn extra() {}\n",
            ),
        ],
        &Expect::exact(&[target("app/api"), target("app/shared")]).note(
            "a test-target change in shared cannot affect shared's dependents; only shared's own \
             tests and the production closure of api are required",
        ),
    );
}

#[test]
fn workspace_manifest_change_widens_only_its_own_workspace() {
    let monorepo = Monorepo::new(vec![
        ws("core").package(pkg("math")),
        ws("sdk").package(pkg("state").dep_at("math", "../../core/math")),
    ]);
    let payload = workspace_level_payload(&monorepo, "core");
    check_with_ferris(
        "workspace_manifest_change_widens_only_its_own_workspace",
        monorepo,
        &[edit(
            "core/Cargo.toml",
            "[workspace]\nmembers = [\"math\"]\nresolver = \"2\"\n# touched\n",
        )],
        &Expect::exact(&[target("core/math")])
            .require_command(CommandExpectation::new("core", CommandMode::WorkspaceWide))
            .forbid_global_fail_wide()
            .note(
                "a workspace manifest has no owning package; widening must stay in that workspace",
            ),
        Some(payload),
    );
}

#[test]
fn workspace_lockfile_change_widens_only_its_own_workspace() {
    let monorepo = Monorepo::new(vec![
        ws("core").package(pkg("math")),
        ws("sdk")
            .lockfile()
            .package(pkg("state").dep_at("math", "../../core/math")),
    ]);
    let lockfile =
        fs::read_to_string(monorepo.root().join("sdk/Cargo.lock")).expect("read lockfile");
    let changed_lockfile = lockfile.replace(
        "name = \"state\"\nversion = \"0.1.0\"",
        "name = \"state\"\nversion = \"0.1.1\"",
    );
    assert_ne!(
        lockfile, changed_lockfile,
        "lockfile bump must change content"
    );
    let payload = workspace_level_payload(&monorepo, "sdk");
    check_with_ferris(
        "workspace_lockfile_change_widens_only_its_own_workspace",
        monorepo,
        &[edit("sdk/Cargo.lock", &changed_lockfile)],
        &Expect::exact(&[target("sdk/state")])
            .require_command(CommandExpectation::new("sdk", CommandMode::WorkspaceWide))
            .forbid_global_fail_wide()
            .note(
                "a workspace lockfile has no owning package; widening must stay in that workspace",
            ),
        Some(payload),
    );
}

/// ferris reports a workspace as affected without naming any crate, the shape a
/// workspace-level input produces.
fn workspace_level_payload(monorepo: &Monorepo, workspace: &str) -> FerrisPayload {
    let workspace_payload = FerrisWorkspace {
        name: workspace.to_string(),
        path: monorepo.workspace_path(workspace),
    };
    FerrisPayload {
        affected_crates: Vec::new(),
        affected_workspaces: vec![workspace_payload.clone()],
        directly_affected_crates: Vec::new(),
        directly_affected_workspaces: vec![workspace_payload],
    }
}

#[test]
fn target_specific_dependency_change_selects_holder() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("helper"))
            .package(pkg("api").manifest_extra(
                "[target.'cfg(unix)'.dependencies]\nhelper = { path = \"../helper\" }",
            ))
            .package(pkg("consumer").dep("api")),
    ]);
    check(
        "target_specific_dependency_change_selects_holder",
        monorepo,
        &[edit(
            "app/helper/src/lib.rs",
            "pub fn helper() -> u32 { 2 }\n",
        )],
        &Expect::exact(&[
            target("app/helper"),
            target("app/api"),
            target("app/consumer"),
        ])
        .note(
            "a target-specific dependency edge is only visible through Cargo metadata, so the \
             index closure must repair the scripted ferris report",
        ),
    );
}

#[test]
fn wrapped_cargo_dispatch_failure_does_not_widen_selection() {
    let monorepo = Monorepo::new(vec![
        ws("core")
            .package(pkg("math"))
            .package(pkg("app").dep("math")),
    ]);
    check_with_wrapped_cargo(
        "wrapped_cargo_dispatch_failure_does_not_widen_selection",
        monorepo,
        &[edit("core/math/src/lib.rs", "pub fn math() -> u32 { 2 }\n")],
        &Expect::exact(&[target("core/math"), target("core/app")])
            .forbid_global_fail_wide()
            .note(
                "direct cargo-ferris-wheel invocation must not depend on Cargo's \
                 external-subcommand dispatch, which fails under build-cache wrappers",
            ),
    );
}

#[test]
fn included_test_source_change_selects_consumers() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(
                pkg("api")
                    .lib_source("include!(\"../tests/shared.rs\");\n\npub fn api() -> u32 { 1 }\n")
                    .file("tests/shared.rs", "pub fn shared_helper() -> u32 { 2 }\n"),
            )
            .package(pkg("consumer").dep("api")),
    ]);
    check(
        "included_test_source_change_selects_consumers",
        monorepo,
        &[edit(
            "app/api/tests/shared.rs",
            "pub fn shared_helper() -> u32 { 3 }\n",
        )],
        &Expect::exact(&[target("app/api"), target("app/consumer")]).note(
            "a test-directory file that the library includes via include! changes the library, so \
             dependents must stay selected",
        ),
    );
}

#[test]
fn integration_test_change_in_leaf_package_stays_local() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("shared").file("tests/it.rs", "#[test]\nfn shared_works() {}\n"))
            .package(pkg("api").dep("shared")),
    ]);
    check(
        "integration_test_change_in_leaf_package_stays_local",
        monorepo,
        &[edit(
            "app/shared/tests/it.rs",
            "#[test]\nfn shared_works() {}\n\n#[test]\nfn shared_also_works() {}\n",
        )],
        &Expect::exact(&[target("app/shared")]).note(
            "integration test targets are not linked into the library, so dependents stay \
             unselected",
        ),
    );
}

#[test]
fn build_script_change_selects_package_and_dependents_only() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("codegen").file("build.rs", "fn main() {}\n"))
            .package(pkg("consumer").dep("codegen"))
            .package(pkg("unrelated")),
    ]);
    check(
        "build_script_change_selects_package_and_dependents_only",
        monorepo,
        &[edit(
            "app/codegen/build.rs",
            "fn main() {\n    // regenerated\n}\n",
        )],
        &Expect::exact(&[target("app/codegen"), target("app/consumer")]).note(
            "build scripts can change generated code for dependents, but not for unrelated crates",
        ),
    );
}

#[test]
fn build_script_change_in_leaf_stays_package_local() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("codegen").file("build.rs", "fn main() {}\n"))
            .package(pkg("unrelated")),
    ]);
    check(
        "build_script_change_in_leaf_stays_package_local",
        monorepo,
        &[edit(
            "app/codegen/build.rs",
            "fn main() {\n    // regenerated\n}\n",
        )],
        &Expect::exact(&[target("app/codegen")]).forbid_global_fail_wide(),
    );
}

#[test]
fn include_macro_source_outside_packages_selects_consumer_closure() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("api").lib_source(
                "include!(\"../../shared/generated.rs\");\n\npub fn api() -> u32 { 1 }\n",
            ))
            .package(pkg("consumer").dep("api"))
            .file("shared/generated.rs", "pub fn generated() -> u32 { 7 }\n"),
    ]);
    check(
        "include_macro_source_outside_packages_selects_consumer_closure",
        monorepo,
        &[edit(
            "app/shared/generated.rs",
            "pub fn generated() -> u32 { 8 }\n",
        )],
        &Expect::exact(&[target("app/api"), target("app/consumer")]).note(
            "an included source outside every package still selects its consumers and their \
             closure",
        ),
    );
}

#[test]
fn include_str_data_file_selects_owning_package_closure() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(
                pkg("api")
                    .lib_source(
                        "const QUERY: &str = include_str!(\"../templates/query.sql\");\n\npub fn \
                         api() -> u32 { 1 }\n",
                    )
                    .file("templates/query.sql", "select 1;\n"),
            )
            .package(pkg("consumer").dep("api")),
    ]);
    check(
        "include_str_data_file_selects_owning_package_closure",
        monorepo,
        &[edit("app/api/templates/query.sql", "select 2;\n")],
        &Expect::exact(&[target("app/api"), target("app/consumer")]).note(
            "included data files select the consuming package and its closure without widening",
        ),
    );
}

#[test]
fn wide_workspace_selects_only_the_dependency_path() {
    let monorepo = Monorepo::new(vec![
        ws("platform")
            .package(pkg("core"))
            .package(pkg("service-a").dep("core"))
            .package(pkg("service-b").dep("service-a"))
            .package(pkg("sibling-one"))
            .package(pkg("sibling-two"))
            .package(pkg("sibling-three")),
    ]);
    check(
        "wide_workspace_selects_only_the_dependency_path",
        monorepo,
        &[edit(
            "platform/core/src/lib.rs",
            "pub fn core() -> u32 { 2 }\n",
        )],
        &Expect::exact(&[
            target("platform/core"),
            target("platform/service-a"),
            target("platform/service-b"),
        ])
        .note("sibling crates in the same workspace are not on the dependency path"),
    );
}

#[test]
fn bench_change_stays_package_local() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("perf").file("benches/perf.rs", "fn main() {}\n"))
            .package(pkg("consumer").dep("perf")),
    ]);
    check(
        "bench_change_stays_package_local",
        monorepo,
        &[edit(
            "app/perf/benches/perf.rs",
            "fn main() {\n    // tuned\n}\n",
        )],
        &Expect::exact(&[target("app/perf")])
            .note("bench targets are not linked into the library, so dependents stay unselected"),
    );
}

#[test]
fn example_change_stays_package_local() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("demo").file("examples/demo.rs", "fn main() {}\n"))
            .package(pkg("consumer").dep("demo")),
    ]);
    check(
        "example_change_stays_package_local",
        monorepo,
        &[edit(
            "app/demo/examples/demo.rs",
            "fn main() {\n    // demo\n}\n",
        )],
        &Expect::exact(&[target("app/demo")])
            .note("example targets are not linked into the library, so dependents stay unselected"),
    );
}

#[test]
fn semantic_manifest_change_keeps_dependents_selected() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("helper"))
            .package(pkg("api"))
            .package(pkg("consumer").dep("api")),
    ]);
    check(
        "semantic_manifest_change_keeps_dependents_selected",
        monorepo,
        &[edit(
            "app/api/Cargo.toml",
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \
             \"2021\"\n\n[dependencies]\nhelper = { path = \"../helper\" }\n",
        )],
        &Expect::exact(&[target("app/api"), target("app/consumer")]).note(
            "adding a dependency can change api's behavior for dependents, so the closure stays",
        ),
    );
}

#[test]
fn inert_manifest_change_stays_package_local() {
    let monorepo = Monorepo::new(vec![
        ws("app")
            .package(pkg("api"))
            .package(pkg("consumer").dep("api")),
    ]);
    check(
        "inert_manifest_change_stays_package_local",
        monorepo,
        &[edit(
            "app/api/Cargo.toml",
            "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2021\"\ndescription = \
             \"api crate\"\n",
        )],
        &Expect::exact(&[target("app/api")])
            .note("edits confined to inert metadata do not change what dependents compile"),
    );
}

#[test]
fn scripted_ferris_under_report_is_repaired_from_index_closure() {
    let monorepo = Monorepo::new(vec![
        ws("core")
            .package(pkg("math"))
            .package(pkg("app").dep("math")),
    ]);
    let workspace_path = monorepo.workspace_path("core");
    let payload = FerrisPayload {
        affected_crates: vec![FerrisCrate {
            name: "math".to_string(),
            workspace: "core".to_string(),
            is_directly_affected: true,
            is_standalone: false,
        }],
        affected_workspaces: vec![FerrisWorkspace {
            name: "core".to_string(),
            path: workspace_path.clone(),
        }],
        directly_affected_crates: vec!["math".to_string()],
        directly_affected_workspaces: vec![FerrisWorkspace {
            name: "core".to_string(),
            path: workspace_path,
        }],
    };
    check_with_ferris(
        "scripted_ferris_under_report_is_repaired_from_index_closure",
        monorepo,
        &[edit("core/math/src/lib.rs", "pub fn math() -> u32 { 2 }\n")],
        &Expect::exact(&[target("core/math"), target("core/app")]).note(
            "when ferris omits a dependent, the index-derived reverse closure must still be \
             selected without widening the workspace",
        ),
        Some(payload),
    );
}
