use std::io::Write;
use std::process::{Command, Stdio};

use serde::Deserialize;

/// Use Pi's own credentials and registry, but never load tools or project files
/// for either an ephemeral reading turn or a Settings catalog query.
pub(crate) fn command() -> Command {
    let mut cmd = Command::new("pi");
    cmd.args([
        "--offline",
        "--no-approve",
        "--no-session",
        "--no-tools",
        "--no-extensions",
        "--no-mcp",
        "--no-context-files",
        "--no-skills",
        "--no-prompt-templates",
    ])
    .env("PI_CACHE_RETENTION", "none")
    .env("PI_TELEMETRY", "0");
    cmd
}

/// Called on a Settings worker, never on the input/render thread. RPC returns
/// exact provider/ID pairs (including custom endpoints), unlike the CLI table.
pub(crate) fn available() -> Result<Vec<String>, String> {
    let cwd =
        tempfile::tempdir().map_err(|e| format!("Cannot create Pi working directory: {e}"))?;
    let mut child = command()
        .args(["--mode", "rpc"])
        .current_dir(cwd.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Cannot start pi: {e}. Is pi on PATH?"))?;
    let sent = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"{\"type\":\"get_available_models\",\"id\":\"catalog\"}\n");
    // EOF ends this read-only RPC session; the child must also be reaped if
    // writing failed (for example, a different pi binary exited immediately).
    let output = child
        .wait_with_output()
        .map_err(|e| format!("Cannot read Pi model list: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "pi exited with {} while listing models. Check your Pi installation.",
            output.status
        ));
    }
    sent.map_err(|e| format!("Cannot request Pi models: {e}"))?;
    catalog(&output.stdout)
}

fn catalog(bytes: &[u8]) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Reply<'a> {
        id: Option<&'a str>,
        success: Option<bool>,
        data: Option<Catalog>,
        error: Option<String>,
    }
    #[derive(Deserialize)]
    struct Catalog {
        models: Vec<Model>,
    }
    #[derive(Deserialize)]
    struct Model {
        provider: String,
        id: String,
    }

    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let reply: Reply =
            serde_json::from_slice(line).map_err(|_| "Invalid Pi catalog response".to_owned())?;
        if reply.id != Some("catalog") {
            continue;
        }
        if reply.success != Some(true) {
            return Err(reply
                .error
                .unwrap_or_else(|| "Pi rejected the model list request".into()));
        }
        let mut names = Vec::new();
        for model in reply.data.ok_or("Pi did not return a model list")?.models {
            if [&model.provider, &model.id]
                .iter()
                .any(|s| s.is_empty() || s.chars().any(char::is_control))
            {
                return Err("Pi returned an invalid provider or model ID".into());
            }
            names.push(format!("{}/{}", model.provider, model.id));
        }
        names.sort_unstable();
        names.dedup();
        return Ok(names);
    }
    Err(
        "No model list received. Install a pi version with get_available_models RPC support."
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::catalog;

    #[test]
    fn pi_catalog_preserves_qualified_model_ids() {
        let reply = br#"{"id":"catalog","success":true,"data":{"models":[{"provider":"gateway","id":"vendor/reader"},{"provider":"direct","id":"vendor/reader"}]}}"#;
        assert_eq!(
            catalog(reply).unwrap(),
            ["direct/vendor/reader", "gateway/vendor/reader"]
        );
    }

    #[test]
    fn pi_catalog_distinguishes_no_models_from_a_failed_fetch() {
        assert!(
            catalog(br#"{"id":"catalog","success":true,"data":{"models":[]}}"#)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            catalog(br#"{"id":"catalog","success":false,"error":"invalid profile"}"#).unwrap_err(),
            "invalid profile"
        );
    }
}
