use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{LitmusError, Result};

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct AffectedWorkspace {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AffectedCrate {
    pub name: String,
    pub workspace: String,
    pub is_directly_affected: bool,
    pub is_standalone: bool,
}

#[derive(Clone, Debug, Deserialize, Default, PartialEq, Serialize)]
pub struct FerrisAffectedReport {
    pub affected_crates: Vec<AffectedCrate>,
    pub affected_workspaces: Vec<AffectedWorkspace>,
    pub directly_affected_crates: Vec<String>,
    pub directly_affected_workspaces: Vec<AffectedWorkspace>,
}

#[derive(Clone, Debug, Deserialize)]
struct LineupReport {
    workspaces: Vec<LineupWorkspace>,
}

#[derive(Clone, Debug, Deserialize)]
struct LineupWorkspace {
    name: String,
    path: String,
}

impl From<LineupWorkspace> for AffectedWorkspace {
    fn from(workspace: LineupWorkspace) -> Self {
        Self {
            name: workspace.name,
            path: workspace.path,
        }
    }
}

pub trait FerrisRunner {
    fn ripples(&self, root: &Path, files: &[String]) -> Result<FerrisAffectedReport>;
    fn lineup(&self, root: &Path) -> Result<FerrisAffectedReport>;
}

#[derive(Default)]
pub struct CommandFerrisRunner;

impl FerrisRunner for CommandFerrisRunner {
    fn ripples(&self, root: &Path, files: &[String]) -> Result<FerrisAffectedReport> {
        let mut args = vec![
            "ferris-wheel".to_string(),
            "ripples".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ];
        args.extend(files.iter().cloned());
        let stdout = run_cargo(root, &args)?;
        parse_ripples_output(&stdout)
    }

    fn lineup(&self, root: &Path) -> Result<FerrisAffectedReport> {
        let stdout = run_cargo(
            root,
            &[
                "ferris-wheel".to_string(),
                "lineup".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ],
        )?;
        let payload = extract_json_block(&stdout)?;
        let report: LineupReport =
            serde_json::from_str(payload).map_err(|source| LitmusError::ParseExternalJson {
                source_name: "cargo ferris-wheel lineup".to_string(),
                message: source.to_string(),
            })?;
        Ok(lineup_to_fail_wide(report))
    }
}

fn run_cargo(root: &Path, args: &[String]) -> Result<String> {
    let output = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|source| LitmusError::CommandIo {
            command: format!("cargo {}", args.join(" ")),
            source,
        })?;

    if !output.status.success() {
        return Err(LitmusError::CommandFailed {
            command: format!("cargo {}", args.join(" ")),
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn parse_ripples_output(raw: &str) -> Result<FerrisAffectedReport> {
    let payload = extract_json_block(raw)?;
    serde_json::from_str(payload).map_err(|source| LitmusError::ParseExternalJson {
        source_name: "cargo ferris-wheel ripples".to_string(),
        message: source.to_string(),
    })
}

fn lineup_to_fail_wide(report: LineupReport) -> FerrisAffectedReport {
    let mut workspaces: Vec<_> = report
        .workspaces
        .into_iter()
        .map(AffectedWorkspace::from)
        .collect();
    workspaces.sort();
    workspaces.dedup();

    FerrisAffectedReport {
        affected_crates: Vec::new(),
        affected_workspaces: workspaces.clone(),
        directly_affected_crates: Vec::new(),
        directly_affected_workspaces: workspaces,
    }
}

fn extract_json_block(raw: &str) -> Result<&str> {
    let start = raw
        .find('{')
        .ok_or_else(|| LitmusError::ParseExternalJson {
            source_name: "ferris-wheel output".to_string(),
            message: "no JSON object found".to_string(),
        })?;
    let end = raw
        .rfind('}')
        .ok_or_else(|| LitmusError::ParseExternalJson {
            source_name: "ferris-wheel output".to_string(),
            message: "no JSON object found".to_string(),
        })?;
    if end < start {
        return Err(LitmusError::ParseExternalJson {
            source_name: "ferris-wheel output".to_string(),
            message: "malformed JSON object bounds".to_string(),
        });
    }
    Ok(&raw[start..=end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ripples_with_prefix_noise() {
        let report = parse_ripples_output(
            r#"warning
{"affected_crates":[{"name":"a","workspace":"w","is_directly_affected":true,"is_standalone":false}],"affected_workspaces":[{"name":"w","path":"w"}],"directly_affected_crates":["a"],"directly_affected_workspaces":[{"name":"w","path":"w"}]}"#,
        )
        .unwrap();

        assert_eq!(report.directly_affected_crates, vec!["a"]);
        assert_eq!(report.affected_workspaces[0].name, "w");
    }

    #[test]
    fn converts_lineup_to_fail_wide_workspaces() {
        let report = lineup_to_fail_wide(LineupReport {
            workspaces: vec![LineupWorkspace {
                name: "nodes".to_string(),
                path: "nodes".to_string(),
            }],
        });

        assert!(report.affected_crates.is_empty());
        assert_eq!(report.affected_workspaces[0].name, "nodes");
        assert_eq!(report.directly_affected_workspaces[0].name, "nodes");
    }
}
