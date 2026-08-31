use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::error::{LitmusError, Result};

pub const CONFIG_FILE_NAME: &str = ".cargo-litmus.toml";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LitmusConfig {
    #[serde(default)]
    pub rules: Vec<InputRule>,
    #[serde(default, rename = "build-script-inputs")]
    pub build_script_inputs: Vec<BuildScriptInput>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildScriptInput {
    pub paths: Vec<String>,
    pub packages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputRule {
    pub paths: Vec<String>,
    #[serde(default)]
    pub workspaces: Vec<String>,
    #[serde(default)]
    pub packages: Vec<String>,
    pub selection: RuleSelection,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSelection {
    Workspace,
    Packages,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfiguredInput {
    pub path: String,
    pub rule_index: usize,
    pub source: ConfiguredInputSource,
    pub selection: RuleSelection,
    pub workspaces: Vec<String>,
    pub packages: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfiguredInputSource {
    Rule,
    BuildScriptInput,
}

struct CompiledRule<'a> {
    index: usize,
    rule: &'a InputRule,
    paths: GlobSet,
}

struct CompiledBuildScriptInput<'a> {
    index: usize,
    input: &'a BuildScriptInput,
    paths: GlobSet,
}

impl LitmusConfig {
    pub fn load_optional(root: &Path) -> Result<Self> {
        let path = root.join(CONFIG_FILE_NAME);
        match fs::read_to_string(&path) {
            Ok(contents) => Self::parse(&path, &contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(LitmusError::io(path, source)),
        }
    }

    pub fn parse(path: &Path, contents: &str) -> Result<Self> {
        let config: Self = toml::from_str(contents).map_err(|source| LitmusError::Config {
            path: path.to_path_buf(),
            message: source.to_string(),
        })?;
        config.validate(path)?;
        Ok(config)
    }

    pub fn matches(&self, paths: impl IntoIterator<Item = String>) -> Result<Vec<ConfiguredInput>> {
        let config_path = Path::new(CONFIG_FILE_NAME);
        let compiled = self.compile(config_path)?;
        let compiled_build_script_inputs = self.compile_build_script_inputs(config_path)?;
        let mut matches = Vec::new();
        for path in paths {
            for rule in &compiled {
                if rule.paths.is_match(&path) {
                    matches.push(ConfiguredInput {
                        path: path.clone(),
                        rule_index: rule.index,
                        source: ConfiguredInputSource::Rule,
                        selection: rule.rule.selection.clone(),
                        workspaces: rule.rule.workspaces.clone(),
                        packages: rule.rule.packages.clone(),
                    });
                }
            }
            for compiled_input in &compiled_build_script_inputs {
                if compiled_input.paths.is_match(&path) {
                    matches.push(ConfiguredInput {
                        path: path.clone(),
                        rule_index: compiled_input.index,
                        source: ConfiguredInputSource::BuildScriptInput,
                        selection: RuleSelection::Packages,
                        workspaces: Vec::new(),
                        packages: compiled_input.input.packages.clone(),
                    });
                }
            }
        }
        matches.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.rule_index.cmp(&right.rule_index))
        });
        Ok(matches)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.paths.is_empty() {
                return Err(config_error(path, index, "paths must not be empty"));
            }
            match rule.selection {
                RuleSelection::Workspace if rule.workspaces.is_empty() => {
                    return Err(config_error(
                        path,
                        index,
                        "workspace selection requires at least one workspace",
                    ));
                }
                RuleSelection::Packages if rule.packages.is_empty() => {
                    return Err(config_error(
                        path,
                        index,
                        "package selection requires at least one package",
                    ));
                }
                _ => {}
            }
        }
        for (index, input) in self.build_script_inputs.iter().enumerate() {
            if input.paths.is_empty() || input.packages.is_empty() {
                return Err(LitmusError::Config {
                    path: path.to_path_buf(),
                    message: format!(
                        "build-script-input {} requires non-empty paths and packages",
                        index + 1
                    ),
                });
            }
        }
        self.compile(path)?;
        self.compile_build_script_inputs(path).map(|_| ())
    }

    fn compile<'a>(&'a self, path: &Path) -> Result<Vec<CompiledRule<'a>>> {
        self.rules
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let mut builder = GlobSetBuilder::new();
                for pattern in &rule.paths {
                    let glob = Glob::new(pattern).map_err(|source| LitmusError::Config {
                        path: path.to_path_buf(),
                        message: format!(
                            "rule {} has invalid glob {pattern:?}: {source}",
                            index + 1
                        ),
                    })?;
                    builder.add(glob);
                }
                let paths = builder.build().map_err(|source| LitmusError::Config {
                    path: path.to_path_buf(),
                    message: format!("rule {} glob set failed: {source}", index + 1),
                })?;
                Ok(CompiledRule { index, rule, paths })
            })
            .collect()
    }

    fn compile_build_script_inputs<'a>(
        &'a self,
        path: &Path,
    ) -> Result<Vec<CompiledBuildScriptInput<'a>>> {
        self.build_script_inputs
            .iter()
            .enumerate()
            .map(|(index, input)| {
                Ok(CompiledBuildScriptInput {
                    index,
                    input,
                    paths: compile_patterns(path, "build-script-input", index, &input.paths)?,
                })
            })
            .collect()
    }
}

fn compile_patterns(
    path: &Path,
    entry_kind: &str,
    index: usize,
    patterns: &[String],
) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|source| LitmusError::Config {
            path: path.to_path_buf(),
            message: format!(
                "{entry_kind} {} has invalid glob {pattern:?}: {source}",
                index + 1
            ),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|source| LitmusError::Config {
        path: path.to_path_buf(),
        message: format!("{entry_kind} {} glob set failed: {source}", index + 1),
    })
}

fn config_error(path: &Path, index: usize, message: &str) -> LitmusError {
    LitmusError::Config {
        path: PathBuf::from(path),
        message: format!("rule {}: {message}", index + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches_repository_rules() {
        let config = LitmusConfig::parse(
            Path::new(".cargo-litmus.toml"),
            r#"
[[rules]]
paths = ["migrations/**"]
workspaces = ["server"]
selection = "workspace"

[[rules]]
paths = ["schemas/**"]
packages = ["api-types", "api-server"]
selection = "packages"
"#,
        )
        .unwrap();

        let matches = config
            .matches([
                "migrations/001.sql".to_string(),
                "schemas/api.json".to_string(),
            ])
            .unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].workspaces, vec!["server"]);
        assert_eq!(matches[1].packages, vec!["api-types", "api-server"]);
    }

    #[test]
    fn rejects_package_rules_without_packages() {
        let error = LitmusConfig::parse(
            Path::new(".cargo-litmus.toml"),
            r#"
[[rules]]
paths = ["schemas/**"]
selection = "packages"
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("requires at least one package"));
    }

    #[test]
    fn matches_declared_build_script_inputs() {
        let config = LitmusConfig::parse(
            Path::new(".cargo-litmus.toml"),
            r#"
[[build-script-inputs]]
paths = ["proto/**", "assets/schema.json"]
packages = ["generated-api"]
"#,
        )
        .unwrap();

        let matches = config.matches(["proto/service.proto".to_string()]).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source, ConfiguredInputSource::BuildScriptInput);
        assert_eq!(matches[0].packages, vec!["generated-api"]);
    }
}
