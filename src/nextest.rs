use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::error::{LitmusError, Result};

pub trait NextestRunner {
    fn list_tests(&self, root: &Path, workspace_path: &str, args: &[String])
    -> Result<Vec<String>>;
}

#[derive(Default)]
pub struct CommandNextestRunner;

impl NextestRunner for CommandNextestRunner {
    fn list_tests(
        &self,
        root: &Path,
        workspace_path: &str,
        args: &[String],
    ) -> Result<Vec<String>> {
        let mut command_args = vec![
            "nextest".to_string(),
            "list".to_string(),
            "--message-format".to_string(),
            "json".to_string(),
        ];
        command_args.extend(args.iter().cloned());
        let output = Command::new("cargo")
            .args(&command_args)
            .current_dir(root.join(workspace_path))
            .output()
            .map_err(|source| LitmusError::CommandIo {
                command: format!("cargo {}", command_args.join(" ")),
                source,
            })?;
        if !output.status.success() {
            return Err(LitmusError::CommandFailed {
                command: format!("cargo {}", command_args.join(" ")),
                code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        parse_nextest_list_json(&String::from_utf8_lossy(&output.stdout))
    }
}

pub fn parse_nextest_list_json(raw: &str) -> Result<Vec<String>> {
    let value: Value =
        serde_json::from_str(raw).map_err(|source| LitmusError::ParseExternalJson {
            source_name: "cargo nextest list".to_string(),
            message: source.to_string(),
        })?;
    let mut tests = Vec::new();
    let Some(suites) = value.get("rust-suites").and_then(Value::as_object) else {
        return Ok(tests);
    };
    for suite in suites.values() {
        let Some(testcases) = suite.get("testcases").and_then(Value::as_object) else {
            continue;
        };
        for (name, testcase) in testcases {
            let matches = testcase
                .get("filter-match")
                .and_then(|filter| filter.get("status"))
                .and_then(Value::as_str)
                == Some("matches");
            if matches {
                tests.push(name.clone());
            }
        }
    }
    tests.sort();
    tests.dedup();
    Ok(tests)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_matching_tests() {
        let tests = parse_nextest_list_json(
            r#"{"rust-suites":{"pkg":{"testcases":{"a":{"filter-match":{"status":"matches"}},"b":{"filter-match":{"status":"mismatch"}}}}}}"#,
        )
        .unwrap();

        assert_eq!(tests, vec!["a"]);
    }
}
