use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::affected::{AffectedReport, InputExplanation, NextestCommand, NextestValidation};
use crate::error::{LitmusError, Result};
use crate::model::IndexMetadata;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExplainSubject {
    Path { path: String },
    Package { package: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SelectionChain {
    pub from: String,
    pub relationship: String,
    pub to: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExplainReport {
    pub schema_version: u32,
    pub subject: ExplainSubject,
    pub input: Option<InputExplanation>,
    pub selection_chains: Vec<SelectionChain>,
    pub selected_packages: Vec<String>,
    pub nextest_commands: Vec<NextestCommand>,
    pub nextest_validation: NextestValidation,
    pub failed_wide: bool,
    pub widening_reasons: Vec<String>,
}

impl ExplainReport {
    pub fn for_path(path: &str, affected: AffectedReport) -> Self {
        let input = affected
            .input_explanations
            .iter()
            .find(|input| input.path == path)
            .cloned();
        let mut selection_chains = input
            .iter()
            .flat_map(|input| {
                input.direct_packages.iter().map(|package| SelectionChain {
                    from: path.to_string(),
                    relationship: input.reason.clone(),
                    to: package.clone(),
                })
            })
            .collect::<Vec<_>>();
        selection_chains.extend(input.iter().flat_map(|input| {
            input.dependency_chains.iter().map(|chain| SelectionChain {
                from: chain.from_package.clone(),
                relationship: chain.relationship.clone(),
                to: chain.to_package.clone(),
            })
        }));
        selection_chains.extend(
            affected
                .nextest_commands
                .iter()
                .map(|command| SelectionChain {
                    from: command.workspace.clone(),
                    relationship: command.reason.clone(),
                    to: format!("cargo nextest {}", command.args.join(" ")),
                }),
        );
        let mut widening_reasons = affected
            .workspace_selections
            .iter()
            .filter(|selection| selection.failed_wide)
            .map(|selection| selection.selection_reason.clone())
            .collect::<BTreeSet<_>>();
        widening_reasons.extend(
            affected
                .test_workspace_selections
                .iter()
                .filter(|selection| selection.failed_wide)
                .map(|selection| selection.selection_reason.clone()),
        );
        widening_reasons.extend(
            affected
                .nextest_commands
                .iter()
                .filter(|command| command.widened_from.is_some())
                .map(|command| command.reason.clone()),
        );
        if affected.failed_wide {
            widening_reasons.insert(affected.selection_reason.clone());
        }
        if affected.test_failed_wide {
            widening_reasons.insert(affected.test_selection_reason.clone());
        }
        let failed_wide = affected.failed_wide || affected.test_failed_wide;
        Self {
            schema_version: 1,
            subject: ExplainSubject::Path {
                path: path.to_string(),
            },
            input,
            selection_chains,
            selected_packages: affected
                .affected_crates
                .iter()
                .map(|krate| krate.name.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            nextest_commands: affected.nextest_commands,
            nextest_validation: affected.nextest_validation,
            failed_wide,
            widening_reasons: widening_reasons.into_iter().collect(),
        }
    }

    pub fn for_package(metadata: &IndexMetadata, package_name: &str) -> Result<Self> {
        let matches = metadata
            .packages
            .iter()
            .filter(|package| package.name == package_name)
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(LitmusError::InvalidArguments {
                message: format!(
                    "expected exactly one package named {package_name:?}, found {}",
                    matches.len()
                ),
            });
        }
        let root = matches[0];
        let packages_by_id = metadata
            .packages
            .iter()
            .map(|package| (package.id.as_str(), package.name.as_str()))
            .collect::<BTreeMap<_, _>>();
        let mut reverse = BTreeMap::<&str, Vec<&str>>::new();
        for dependency in &metadata.package_dependencies {
            reverse
                .entry(&dependency.depends_on_package_id)
                .or_default()
                .push(&dependency.package_id);
        }
        let mut visited = BTreeSet::from([root.id.as_str()]);
        let mut pending = VecDeque::from([root.id.as_str()]);
        let mut chains = Vec::new();
        while let Some(current) = pending.pop_front() {
            for dependent in reverse.get(current).into_iter().flatten() {
                if visited.insert(dependent) {
                    pending.push_back(dependent);
                    chains.push(SelectionChain {
                        from: packages_by_id
                            .get(current)
                            .copied()
                            .unwrap_or(current)
                            .to_string(),
                        relationship: "reverse Cargo dependency".to_string(),
                        to: packages_by_id
                            .get(dependent)
                            .copied()
                            .unwrap_or(dependent)
                            .to_string(),
                    });
                }
            }
        }
        let selected_packages = visited
            .into_iter()
            .filter_map(|id| packages_by_id.get(id).copied())
            .map(str::to_string)
            .collect();
        Ok(Self {
            schema_version: 1,
            subject: ExplainSubject::Package {
                package: package_name.to_string(),
            },
            input: None,
            selection_chains: chains,
            selected_packages,
            nextest_commands: Vec::new(),
            nextest_validation: NextestValidation::Skipped,
            failed_wide: false,
            widening_reasons: Vec::new(),
        })
    }
}
