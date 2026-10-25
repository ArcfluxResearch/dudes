//! The Codex release lines whose app-server protocol the streaming reader speaks.

/// Streaming is verified on each of these `major.minor` lines. Their patch releases keep the
/// protocol. Only the second line has verified pre-releases.
const LINES: [&str; 2] = ["0.160", "0.164"];

/// The installed Codex is outside the supported line, named as it reported itself.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Codex streaming requires codex-cli {first}.x or {second}.x, but the installed Codex reports {reported}", first = LINES[0], second = LINES[1])]
pub struct UnsupportedVersion {
    reported: String,
}

/// Admit the output of `codex --version`, `codex-cli <version>`.
///
/// # Errors
/// The output is not in that form or names a version outside the supported line.
pub fn check_cli(output: &str) -> Result<(), UnsupportedVersion> {
    let output = output.trim();
    check(output, output.strip_prefix("codex-cli "))
}

/// Admit the app-server's `initialize` user agent, `dudes/<version> (<platform>) ...`.
pub(crate) fn check_user_agent(agent: &str) -> Result<(), UnsupportedVersion> {
    let product = agent.split_once(' ').map_or(agent, |(product, _)| product);
    check(product, product.strip_prefix("dudes/"))
}

fn check(reported: &str, version: Option<&str>) -> Result<(), UnsupportedVersion> {
    if version.is_some_and(in_line) {
        Ok(())
    } else {
        Err(UnsupportedVersion {
            reported: super::reported(reported),
        })
    }
}

fn in_line(version: &str) -> bool {
    LINES.iter().enumerate().any(|(index, line)| {
        version
            .strip_prefix(line)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|rest| {
                let (patch, pre) = rest
                    .split_once('-')
                    .map_or((rest, None), |(patch, pre)| (patch, Some(pre)));
                (patch == "0" || !patch.starts_with('0'))
                    && !patch.is_empty()
                    && patch.bytes().all(|byte| byte.is_ascii_digit())
                    && pre.is_none_or(|pre| index == 1 && dotted_alphanumeric(pre))
            })
    })
}

/// Non-empty alphanumeric segments separated by single dots.
fn dotted_alphanumeric(text: &str) -> bool {
    text.split('.').all(|segment| {
        !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(reported: &str) -> String {
        format!(
            "Codex streaming requires codex-cli 0.160.x or 0.164.x, but the installed Codex reports {reported}"
        )
    }

    #[test]
    fn patch_releases_of_each_line_are_admitted_and_anything_else_is_named() {
        for output in [
            "codex-cli 0.160.0\n",
            "codex-cli 0.160.1",
            "codex-cli 0.160.10\n",
            "codex-cli 0.164.0",
            "codex-cli 0.164.3\n",
            "codex-cli 0.164.0-alpha.11.1\n",
            "codex-cli 0.164.2-rc1",
        ] {
            assert_eq!(check_cli(output), Ok(()), "{output}");
        }
        let long = format!("codex-cli 0.161.0 {}", "x".repeat(80));
        for (output, reported) in [
            ("codex-cli 0.161.0\n", "codex-cli 0.161.0".to_owned()),
            ("codex-cli 0.163.9", "codex-cli 0.163.9".to_owned()),
            ("codex-cli 0.165.0", "codex-cli 0.165.0".to_owned()),
            ("codex-cli 0.164.0-", "codex-cli 0.164.0-".to_owned()),
            ("codex-cli 0.164.0-a b", "codex-cli 0.164.0-a b".to_owned()),
            (
                "codex-cli 0.160.1-alpha.1",
                "codex-cli 0.160.1-alpha.1".to_owned(),
            ),
            (
                "codex-cli 0.164.0-a..b",
                "codex-cli 0.164.0-a..b".to_owned(),
            ),
            ("codex-cli 0.164.0-.a", "codex-cli 0.164.0-.a".to_owned()),
            ("codex-cli 0.164.0-a.", "codex-cli 0.164.0-a.".to_owned()),
            ("codex-cli 0.164.0-a_b", "codex-cli 0.164.0-a_b".to_owned()),
            ("codex-cli 0.159.9", "codex-cli 0.159.9".to_owned()),
            ("codex-cli 1.160.0", "codex-cli 1.160.0".to_owned()),
            ("codex-cli 0.1600.0", "codex-cli 0.1600.0".to_owned()),
            ("codex-cli 0.160", "codex-cli 0.160".to_owned()),
            ("codex-cli 0.160.", "codex-cli 0.160.".to_owned()),
            ("codex-cli 0.160.01", "codex-cli 0.160.01".to_owned()),
            ("0.160.1", "0.160.1".to_owned()),
            (
                "codex 0.160.1\u{1b}[2J",
                "codex 0.160.1\\u{1b}[2J".to_owned(),
            ),
            (long.as_str(), format!("{}…", &long[..64])),
        ] {
            assert_eq!(
                check_cli(output).unwrap_err().to_string(),
                refusal(&reported),
                "{output}"
            );
        }
    }

    #[test]
    fn the_user_agent_names_the_running_version() {
        assert_eq!(
            check_user_agent("dudes/0.160.1 (Mac OS 14.1.1; arm64) WarpTerminal (dudes; 1)"),
            Ok(())
        );
        assert_eq!(
            check_user_agent(
                "dudes/0.164.0-alpha.11.1 (Mac OS 14.1.1; arm64) Terminal/1 (dudes; 1)"
            ),
            Ok(())
        );
        for (agent, reported) in [
            ("dudes/0.161.0 (Mac OS 14.1.1; arm64)", "dudes/0.161.0"),
            ("dudes/0.160.01 (Mac OS 14.1.1; arm64)", "dudes/0.160.01"),
            ("dudes/ 0.160.1 (Mac OS 14.1.1; arm64)", "dudes/"),
            ("other/0.160.1 (Mac OS 14.1.1; arm64)", "other/0.160.1"),
            ("0.160.1", "0.160.1"),
        ] {
            assert_eq!(
                check_user_agent(agent).unwrap_err().to_string(),
                refusal(reported),
                "{agent}"
            );
        }
    }
}
