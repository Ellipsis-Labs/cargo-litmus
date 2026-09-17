//! Test-side mirror of the CLI's stable JSON report contract.
//!
//! Only the fields the harness reads are mirrored; serde ignores the rest of
//! the report.

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Report {
    pub failed_wide: bool,
    pub selection_reason: String,
    pub test_failed_wide: bool,
    pub test_selection_reason: String,
    pub unknown_files: Vec<String>,
    pub changed_files: Vec<ChangedFile>,
    pub selected_test_packages: Vec<SelectedPackage>,
    pub nextest_commands: Vec<NextestCommand>,
    pub test_workspace_selections: Vec<WorkspaceSelection>,
    pub test_selection_checks: Vec<SelectionCheck>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkspaceRef {
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SelectedPackage {
    pub name: String,
    pub workspace: String,
    pub reason: String,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandMode {
    Module,
    TestTarget,
    Package,
    WorkspaceWide,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NextestCommand {
    pub workspace: String,
    pub args: Vec<String>,
    pub mode: CommandMode,
    pub module_filter: Option<String>,
    pub widened_from: Option<CommandMode>,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkspaceSelection {
    pub workspace: WorkspaceRef,
    pub failed_wide: bool,
    pub selection_reason: String,
    pub unknown_files: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SelectionCheck {
    pub changed_package: PackageRef,
    pub missing_dependents: Vec<PackageRef>,
    pub result: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PackageRef {
    pub name: String,
    pub workspace: String,
}

impl NextestCommand {
    /// Every `-p <package>` argument; litmus batches multiple packages into one
    /// nextest invocation per workspace.
    pub fn package_args(&self) -> Vec<&str> {
        self.args
            .windows(2)
            .filter(|window| window[0] == "-p")
            .map(|window| window[1].as_str())
            .collect()
    }
}
