use serde_json::{Value, json};

use super::{Phase, Turn};

// Pi RPC has no ready banner. Commands may be written immediately. Unlike the
// CLI's --model pattern matching, set_model selects an exact provider and ID
// for this process without changing the user's default.
pub(super) fn start(
    turn: &mut Turn,
    model: Option<&str>,
    send: impl FnOnce(&Value) -> Result<(), String>,
) -> Result<(), String> {
    let Some(model) = model else {
        return turn.preflight(send);
    };
    let (provider, id) = model
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
        .ok_or("Invalid Pi model in Settings (expected provider/id). No question was sent.")?;
    send(&json!({"type":"set_model", "id":"model", "provider":provider, "modelId":id}))?;
    turn.phase = Phase::SelectModel;
    Ok(())
}

pub(super) fn advance(
    turn: &mut Turn,
    kind: &str,
    frame: &Value,
    send: impl FnOnce(&Value) -> Result<(), String>,
) -> Result<bool, String> {
    match kind {
        "response" => {
            if frame["success"] != true {
                return Err(format!(
                    "Pi: {}",
                    frame["error"].as_str().unwrap_or("RPC request failed")
                ));
            }
            match (turn.phase, frame["command"].as_str(), frame["id"].as_str()) {
                (Phase::SelectModel, Some("set_model"), Some("model")) => turn.preflight(send)?,
                (Phase::Preflight, Some("get_state"), Some("state")) => {
                    let state = &frame["data"];
                    if !["provider", "id"].iter().all(|key| {
                        state["model"][key]
                            .as_str()
                            .is_some_and(|name| !name.is_empty())
                    }) {
                        return Err("Pi has no model selected. Configure a model and login in pi. No question was sent.".into());
                    }
                    if state["messageCount"].as_u64() != Some(0) || !state["sessionFile"].is_null()
                    {
                        return Err(
                            "Pi did not open a fresh, unsaved session. No question was sent."
                                .into(),
                        );
                    }
                    turn.send_prompt(send)?;
                }
                (Phase::AwaitingPrompt, Some("prompt"), Some(id)) if id == turn.id => {
                    if frame["data"]["disposition"] != "started" {
                        return Err("Pi did not start a completion for this question".into());
                    }
                    turn.phase = Phase::Running;
                }
                _ => (),
            }
        }
        "extension_ui_request" | "tool_execution_start" => {
            return Err("Unexpected tool or extension request in a tool-free Pi session".into());
        }
        "extension_error" => {
            return Err(frame["error"]
                .as_str()
                .unwrap_or("Pi extension failed")
                .into());
        }
        "message_update"
            if turn.phase == Phase::Running
                && frame["assistantMessageEvent"]["type"] == "text_delta" =>
        {
            if let Some(text) = frame["assistantMessageEvent"]["delta"].as_str() {
                turn.text(text);
            }
        }
        "message_end"
            if turn.phase == Phase::Running && frame["message"]["role"] == "assistant" =>
        {
            let msg = &frame["message"];
            turn.finish_text(msg);
            match msg["stopReason"].as_str() {
                Some("stop" | "length") => turn.phase = Phase::Settling,
                Some("error" | "aborted") => {
                    return Err(msg["errorMessage"]
                        .as_str()
                        .unwrap_or("Pi completion failed or was aborted")
                        .into());
                }
                _ => return Err("Pi did not finish a tool-free completion".into()),
            }
        }
        // agent_end is only the end of one low-level run. Pi 1.5 emits
        // agent_settled when no further automatic work remains.
        "agent_settled" => {
            if frame["aborted"] != false {
                return Err("Pi aborted the request".into());
            }
            if turn.phase != Phase::Settling || !turn.prior_text {
                return Err("Pi ended without an assistant answer".into());
            }
            return Ok(true);
        }
        _ => (),
    }
    Ok(false)
}
