//! Host capability, subscription login, and native resume checks.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

use super::process::{bounded_error, check_api_override, ssh_command};
use super::{RunRequest, Target};
use crate::sandbox::ssh_command::{shell_quote, shell_quote_path};

#[derive(Debug, Clone)]
pub struct PreflightStatus {
    pub available: bool,
    pub authenticated: bool,
    pub version: Option<String>,
    pub reason: Option<String>,
}

pub async fn preflight(
    target: &Target,
    cwd: &Path,
    executable: &str,
    config_dir: Option<&Path>,
) -> Result<PreflightStatus> {
    if matches!(target, Target::Local) {
        check_api_override()?;
    }
    let remote_override_check = "for name in ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL CLAUDE_CODE_OAUTH_TOKEN CLAUDE_CODE_SESSION_ACCESS_TOKEN CLAUDE_CODE_HOST_SESSION_ID CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_VERTEX CLAUDE_CODE_USE_FOUNDRY; do eval 'present=${'\"$name\"'+x}'; if [ \"$present\" = x ]; then printf 'API/provider override set: %s\\n' \"$name\" >&2; exit 86; fi; done; for name in $(env | cut -d= -f1); do case \"$name\" in CLAUDE_CODE_*_TOKEN|ANTHROPIC_*_TOKEN|ANTHROPIC_*_KEY) printf 'API/provider override set: %s\\n' \"$name\" >&2; exit 86;; esac; done; ";
    let output = match target {
        Target::Local => {
            let mut command = Command::new(executable);
            command.current_dir(cwd).arg("--version");
            command.env_remove("ANTHROPIC_API_KEY");
            command.env_remove("ANTHROPIC_AUTH_TOKEN");
            if let Some(dir) = config_dir {
                command.env("CLAUDE_CONFIG_DIR", dir);
            } else {
                command.env_remove("CLAUDE_CONFIG_DIR");
            }
            command.kill_on_drop(true);
            timeout(Duration::from_secs(15), command.output())
                .await
                .context("local Claude preflight timed out")??
        }
        Target::Ssh(connection) => {
            let env = config_dir
                .map(|dir| {
                    format!(
                "config_dir={} && env -u CLAUDE_CONFIG_DIR CLAUDE_CONFIG_DIR=\"$config_dir\" ",
                shell_quote_path(&dir.display().to_string())
            )
                })
                .unwrap_or_else(|| "env -u CLAUDE_CONFIG_DIR ".to_string());
            let remote = format!(
                "cd {} && {}{}{} --version",
                shell_quote_path(&cwd.display().to_string()),
                remote_override_check,
                env,
                shell_quote(executable)
            );
            let mut command = ssh_command(connection, &remote)?;
            command.stdin(Stdio::null());
            timeout(Duration::from_secs(15), command.output())
                .await
                .context("SSH Claude preflight timed out")??
        }
    };
    if !output.status.success() {
        bail!("Claude Code unavailable: {}", bounded_error(&output.stderr));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        bail!("Claude Code version check returned no version");
    }
    // 2.1.274 reports a valid subscription login but rejects the current
    // default model. Admit only releases that support that model.
    let runtime_supported = version
        .split_whitespace()
        .next()
        .and_then(|part| {
            let mut numbers = part.split('.').map(str::parse::<u32>);
            Some((
                numbers.next()?.ok()?,
                numbers.next()?.ok()?,
                numbers.next()?.ok()?,
            ))
        })
        .is_some_and(|found| found >= (2, 1, 280));
    if !runtime_supported {
        return Ok(PreflightStatus {
            available: true,
            authenticated: false,
            version: Some(version),
            reason: Some("Claude Code 2.1.280 or newer is required for the current default model; update the selected host executable".to_string()),
        });
    }
    let auth = match target {
        Target::Local => {
            let mut command = Command::new(executable);
            command
                .current_dir(cwd)
                .args(["auth", "status"])
                .kill_on_drop(true);
            if let Some(dir) = config_dir {
                command.env("CLAUDE_CONFIG_DIR", dir);
            } else {
                command.env_remove("CLAUDE_CONFIG_DIR");
            }
            timeout(Duration::from_secs(15), command.output())
                .await
                .context("Claude login status timed out")??
        }
        Target::Ssh(connection) => {
            let env = config_dir
                .map(|dir| {
                    format!(
                "config_dir={} && env -u CLAUDE_CONFIG_DIR CLAUDE_CONFIG_DIR=\"$config_dir\" ",
                shell_quote_path(&dir.display().to_string())
            )
                })
                .unwrap_or_else(|| "env -u CLAUDE_CONFIG_DIR ".to_string());
            let remote = format!(
                "cd {} && {}{}{} auth status",
                shell_quote_path(&cwd.display().to_string()),
                remote_override_check,
                env,
                shell_quote(executable)
            );
            let mut command = ssh_command(connection, &remote)?;
            command.stdin(Stdio::null());
            timeout(Duration::from_secs(15), command.output())
                .await
                .context("SSH Claude login status timed out")??
        }
    };
    if !auth.status.success() {
        return Ok(PreflightStatus {
            available: true,
            authenticated: false,
            version: Some(version),
            reason: Some(format!(
                "Claude login status failed: {}",
                bounded_error(&auth.stderr)
            )),
        });
    }
    let account: Value =
        serde_json::from_slice(&auth.stdout).context("Claude login status is not JSON")?;
    let logged_in = account.get("loggedIn").and_then(Value::as_bool) == Some(true);
    let subscription = account.get("authMethod").and_then(Value::as_str) == Some("claude.ai")
        && account.get("apiProvider").and_then(Value::as_str) == Some("firstParty");
    let authenticated = logged_in && subscription;
    Ok(PreflightStatus {
        available: true,
        authenticated,
        version: Some(version),
        reason: (!authenticated).then(|| {
            "Claude Code is not signed in with a first-party subscription on this host".to_string()
        }),
    })
}

pub(super) async fn verify_native_transcript(request: &RunRequest) -> Result<()> {
    let Some(id) = request.resume_id.as_deref() else {
        return Ok(());
    };
    let id = uuid::Uuid::parse_str(id).context("Claude resume ID is not a UUID")?;
    let filename = format!("{id}.jsonl");
    let missing = || {
        anyhow!(
        "Claude native transcript {id} is unavailable on the selected host/configuration; restore its Claude Code session file or start a new NAC session"
    )
    };
    match &request.target {
        Target::Local => {
            let expected_cwd = std::fs::canonicalize(&request.cwd).with_context(|| {
                format!("Claude workspace {} is unavailable", request.cwd.display())
            })?;
            let config = match &request.config_dir {
                Some(path) => path.clone(),
                None => std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow!("HOME is needed to find Claude native transcripts"))?
                    .join(".claude"),
            };
            let mut projects = tokio::fs::read_dir(config.join("projects"))
                .await
                .map_err(|_| missing())?;
            while let Some(entry) = projects.next_entry().await? {
                let file = entry.path().join(&filename);
                if tokio::fs::metadata(&file)
                    .await
                    .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
                    && transcript_matches_cwd(&file, &expected_cwd)?
                {
                    return Ok(());
                }
            }
            Err(missing())
        }
        Target::Ssh(connection) => {
            let base = request
                .config_dir
                .as_deref()
                .map(|path| shell_quote_path(&path.display().to_string()))
                .unwrap_or_else(|| "\"$HOME/.claude\"".to_string());
            // Transfer only a bounded transcript prefix. JSON parsing here
            // accepts normal whitespace/escaping and requires a top-level cwd
            // field; grep could match a quoted prompt or reject valid JSON.
            let remote = format!(
                "cd {} && base={} && printf '%s\\0' \"$(pwd -P)\" && count=0; for f in \"$base\"/projects/*/{}; do [ -s \"$f\" ] || continue; head -n 20 \"$f\" | head -c 65536; count=$((count + 1)); [ \"$count\" -lt 50 ] || break; done",
                shell_quote_path(&request.cwd.display().to_string()), base, shell_quote(&filename)
            );
            let mut command = ssh_command(connection, &remote)?;
            command.stdin(Stdio::null());
            let output = timeout(Duration::from_secs(15), command.output())
                .await
                .context("SSH Claude transcript check timed out")??;
            if !output.status.success() {
                return Err(missing());
            }
            let Some(separator) = output.stdout.iter().position(|byte| *byte == 0) else {
                return Err(missing());
            };
            let remote_cwd = std::str::from_utf8(&output.stdout[..separator])
                .context("SSH Claude workspace path is not UTF-8")?;
            let events = &output.stdout[separator + 1..];
            for line in events.split(|byte| *byte == b'\n') {
                if let Ok(event) = serde_json::from_slice::<Value>(line) {
                    if event.get("cwd").and_then(Value::as_str) == Some(remote_cwd) {
                        return Ok(());
                    }
                }
            }
            Err(missing())
        }
    }
}

fn transcript_matches_cwd(file: &Path, expected: &Path) -> Result<bool> {
    let stream = std::fs::File::open(file)?;
    for line in std::io::BufReader::new(stream).lines().take(20) {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = event.get("cwd").and_then(Value::as_str) {
            return Ok(Path::new(cwd) == expected);
        }
    }
    Ok(false)
}
