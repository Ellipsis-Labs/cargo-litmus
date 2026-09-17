//! Two-sided scenario expectations and their violation diagnostics.
//!
//! `required` packages fail the run when they are missing (a false negative);
//! packages outside `allowed` fail the run when they are covered
//! (over-selection past the scenario's declared conservative allowance).

use std::collections::BTreeSet;

use super::monorepo::Monorepo;
use super::report::{CommandMode, Report};

/// A `workspace/package` expectation target.
pub fn target(spec: &str) -> String {
    spec.to_string()
}

pub struct Expect {
    required: BTreeSet<String>,
    allowed: BTreeSet<String>,
    required_commands: Vec<CommandExpectation>,
    forbid_global_fail_wide: bool,
    note: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CommandExpectation {
    workspace: String,
    mode: CommandMode,
    package: Option<String>,
}

impl CommandExpectation {
    pub fn new(workspace: &str, mode: CommandMode) -> Self {
        Self {
            workspace: workspace.to_string(),
            mode,
            package: None,
        }
    }

    pub fn package(mut self, package: &str) -> Self {
        self.package = Some(package.to_string());
        self
    }
}

impl Expect {
    /// Every listed package must be covered, and nothing else may be covered.
    pub fn exact<S: AsRef<str>>(packages: &[S]) -> Self {
        let set: BTreeSet<String> = packages.iter().map(|p| p.as_ref().to_string()).collect();
        Self {
            required: set.clone(),
            allowed: set,
            required_commands: Vec::new(),
            forbid_global_fail_wide: false,
            note: None,
        }
    }

    pub fn require_command(mut self, expectation: CommandExpectation) -> Self {
        self.required_commands.push(expectation);
        self
    }

    pub fn forbid_global_fail_wide(mut self) -> Self {
        self.forbid_global_fail_wide = true;
        self
    }

    pub fn note(mut self, note: &str) -> Self {
        self.note = Some(note.to_string());
        self
    }
}

fn covered_packages(monorepo: &Monorepo, report: &Report) -> BTreeSet<String> {
    let mut covered = BTreeSet::new();
    for command in &report.nextest_commands {
        match command.mode {
            CommandMode::WorkspaceWide => {
                for workspace in monorepo.spec() {
                    if workspace.name == command.workspace {
                        for package in &workspace.packages {
                            covered.insert(format!("{}/{}", workspace.name, package.name));
                        }
                    }
                }
            }
            _ => {
                for package in command.package_args() {
                    covered.insert(format!("{}/{}", command.workspace, package));
                }
            }
        }
    }
    covered
}

pub(crate) fn assert_expectations(
    name: &str,
    monorepo: &Monorepo,
    report: &Report,
    expect: &Expect,
) {
    let covered = covered_packages(monorepo, report);
    let required: BTreeSet<String> = expect.required.clone();
    let allowed: BTreeSet<String> = expect.allowed.clone();

    let mut violations = Vec::new();
    for missing in required.difference(&covered) {
        violations.push(format!(
            "false negative: required package {missing} is not covered"
        ));
    }
    for extra in covered.difference(&allowed) {
        violations.push(format!(
            "over-selection: package {extra} is covered but not allowed"
        ));
    }
    if expect.forbid_global_fail_wide && report.failed_wide {
        violations.push(format!(
            "unexpected global fail-wide: {}",
            report.selection_reason
        ));
    }
    for expected in &expect.required_commands {
        let matched = report.nextest_commands.iter().any(|command| {
            command.workspace == expected.workspace
                && command.mode == expected.mode
                && expected
                    .package
                    .as_ref()
                    .is_none_or(|package| command.package_args().contains(&package.as_str()))
        });
        if !matched {
            violations.push(format!(
                "missing required command: workspace={} mode={:?} package={:?}",
                expected.workspace, expected.mode, expected.package
            ));
        }
    }

    if violations.is_empty() {
        return;
    }

    let mut diagnostic = format!("scenario {name} failed expectations:\n");
    for violation in &violations {
        diagnostic.push_str(&format!("  - {violation}\n"));
    }
    if let Some(note) = &expect.note {
        diagnostic.push_str(&format!("  note: {note}\n"));
    }
    diagnostic.push_str(&format!(
        "\n  failed_wide={} selection_reason={:?}\n  test_failed_wide={} \
         test_selection_reason={:?}\n",
        report.failed_wide,
        report.selection_reason,
        report.test_failed_wide,
        report.test_selection_reason
    ));
    diagnostic.push_str(&format!("  unknown_files={:?}\n", report.unknown_files));
    diagnostic.push_str(&format!(
        "  changed_files={:?}\n",
        report
            .changed_files
            .iter()
            .map(|file| format!("{}:{}", file.path, file.status))
            .collect::<Vec<_>>()
    ));
    diagnostic.push_str("  selected_test_packages:\n");
    for package in &report.selected_test_packages {
        diagnostic.push_str(&format!(
            "    {}/{} ({})\n",
            package.workspace, package.name, package.reason
        ));
    }
    diagnostic.push_str("  nextest_commands:\n");
    for command in &report.nextest_commands {
        diagnostic.push_str(&format!(
            "    workspace={} mode={:?} args={:?} module_filter={:?} widened_from={:?} \
             reason={:?}\n",
            command.workspace,
            command.mode,
            command.args,
            command.module_filter,
            command.widened_from,
            command.reason
        ));
    }
    diagnostic.push_str("  test_workspace_selections:\n");
    for selection in &report.test_workspace_selections {
        diagnostic.push_str(&format!(
            "    {} failed_wide={} reason={:?} unknown_files={:?}\n",
            selection.workspace.name,
            selection.failed_wide,
            selection.selection_reason,
            selection.unknown_files
        ));
    }
    if !report.test_selection_checks.is_empty() {
        diagnostic.push_str("  test_selection_checks:\n");
        for check in &report.test_selection_checks {
            diagnostic.push_str(&format!(
                "    changed={}/{} result={} missing_dependents={:?}\n",
                check.changed_package.workspace,
                check.changed_package.name,
                check.result,
                check
                    .missing_dependents
                    .iter()
                    .map(|package| format!("{}/{}", package.workspace, package.name))
                    .collect::<Vec<_>>()
            ));
        }
    }
    panic!("{diagnostic}");
}
