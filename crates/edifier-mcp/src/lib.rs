//! MCP (Model Context Protocol) server for Edifier headsets.
//!
//! The server speaks newline-delimited JSON-RPC 2.0 over stdio, exposes the tool set described in
//! [`tools::specs`], and keeps one BLE control link open while it runs. Diagnostics go to stderr;
//! stdout carries protocol messages only.

mod jsonrpc;
mod link;
mod tools;

use anyhow::Result;
use edifier_proto::Command;
use jsonrpc::{ErrorResponse, Request, Response, code};
use link::DeviceLink;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tools::Action;
use tracing::{debug, error, warn};

/// MCP protocol revision used when the client does not name one.
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";
/// Server name reported during `initialize`.
pub const SERVER_NAME: &str = "edifier-mcp";
/// Server version reported during `initialize`.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Guidance handed to the client during `initialize`.
const INSTRUCTIONS: &str = "\
Control an Edifier headset (W820NB family) attached to this Mac over its BLE control service.

Workflow: call edifier_connect (or edifier_scan first when several headsets are around) to open the
control link, then edifier_status, edifier_anc, edifier_ambient_volume, edifier_eq, edifier_game_mode,
edifier_prompt_volume, edifier_gestures, edifier_shutdown_timer, edifier_rename, edifier_playback,
edifier_ldac or edifier_raw. edifier_connect_audio moves the classic Bluetooth audio link back to
this Mac; edifier_disconnect closes the control link.

Notes specific to macOS:
- Noise control is driven over BLE, not through Apple's Control Center panel: macOS only offers its
  own noise-control UI for Apple/Beats headsets because that UI is fed by Apple's proprietary
  accessory protocol, which this headset does not implement.
- macOS has no LDAC encoder, so edifier_ldac with `48k`/`96k` silences the audio link until the mode
  is set back to `off`; the BLE control link keeps working, so it is always recoverable.
- Battery, firmware and ANC state are read from the headset, not from macOS.";

/// Serve MCP over stdio until the client closes the pipe.
///
/// `selector` is an optional name fragment or CoreBluetooth identifier of the headset to control.
///
/// # Errors
///
/// Fails when stdin/stdout cannot be read or written.
pub async fn serve_stdio(selector: Option<String>) -> Result<()> {
    let mut link = DeviceLink::new(selector);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let request = match Request::parse(&line) {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(err) => {
                warn!(error = %err, "invalid JSON on stdin");
                let reply = ErrorResponse::new(Value::Null, code::PARSE_ERROR, err.to_string());
                write_line(&mut stdout, &reply.to_line()?).await?;
                continue;
            }
        };
        debug!(method = %request.method, "request");

        let Some(reply) = handle(&mut link, &request).await else {
            continue;
        };
        write_line(&mut stdout, &reply).await?;
    }

    link.close().await;
    Ok(())
}

async fn write_line(stdout: &mut tokio::io::Stdout, line: &str) -> Result<()> {
    stdout.write_all(line.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

/// Dispatch one request, returning the rendered reply line (or `None` for notifications).
async fn handle(link: &mut DeviceLink, request: &Request) -> Option<String> {
    if !request.expects_reply() {
        // Only notifications we know about are expected; unknown ones are ignored by design.
        return None;
    }
    let id = request.id.clone().unwrap_or(Value::Null);
    if request.jsonrpc != "2.0" {
        let reply = ErrorResponse::new(
            id,
            code::INVALID_REQUEST,
            format!("unsupported jsonrpc version {:?}", request.jsonrpc),
        );
        return reply.to_line().ok();
    }

    let result: std::result::Result<Value, (i64, String)> = match request.method.as_str() {
        "initialize" => Ok(initialize_result(&request.params)),
        "ping" => Ok(json!({})),
        "tools/list" => {
            Ok(json!({ "tools": tools::specs().iter().map(tool_descriptor).collect::<Vec<_>>() }))
        }
        "tools/call" => match call_tool(link, &request.params).await {
            Ok(result) => Ok(result),
            Err(err) => Err((code::INVALID_PARAMS, format!("{err:#}"))),
        },
        other => Err((code::METHOD_NOT_FOUND, format!("unknown method {other:?}"))),
    };

    let line = match result {
        Ok(result) => Response::new(id, result).to_line(),
        Err((code, message)) => {
            if code == code::INVALID_PARAMS {
                warn!(%message, "rejected request");
            }
            ErrorResponse::new(id, code, message).to_line()
        }
    };
    match line {
        Ok(line) => Some(line),
        Err(err) => {
            error!(error = %err, "cannot serialize reply");
            None
        }
    }
}

fn initialize_result(params: &Value) -> Value {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty())
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
        "instructions": INSTRUCTIONS,
    })
}

fn tool_descriptor(spec: &tools::ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "inputSchema": spec.schema,
    })
}

/// Resolve and run one `tools/call`.
async fn call_tool(link: &mut DeviceLink, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let action = tools::resolve(name, &arguments)?;

    match execute(link, action).await {
        Ok(result) => Ok(result),
        Err(err) => Ok(json!({
            "content": [{"type": "text", "text": format!("{err:#}")}],
            "isError": true,
        })),
    }
}

async fn execute(link: &mut DeviceLink, action: Action) -> Result<Value> {
    match action {
        Action::Scan { seconds } => {
            let devices = DeviceLink::scan(seconds).await?;
            let text = if devices.is_empty() {
                "no Edifier endpoint found".to_string()
            } else {
                devices
                    .iter()
                    .map(|device| {
                        format!(
                            "{} ({}) rssi={:?} model={}",
                            device.name.as_deref().unwrap_or("?"),
                            device.identifier,
                            device.rssi,
                            device.model.display_name()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(text_result(text, json!({ "devices": devices })))
        }
        Action::Connect { device } => {
            if device.is_some() {
                link.set_selector(device).await;
            }
            let status = link.session().await?.refresh().await;
            let session = link.session().await?;
            let link_description = session.describe().to_string();
            let selector = link.selector().map(str::to_string);
            Ok(text_result(
                format!("{link_description}\n{}", status.summary()),
                json!({
                    "link": link_description,
                    "selector": selector,
                    "status": status,
                }),
            ))
        }
        Action::CloseLink => {
            link.close().await;
            Ok(text_result(
                "control link closed".to_string(),
                json!({ "closed": true }),
            ))
        }
        Action::Status { refresh } => {
            let session = link.session().await?;
            let status = if refresh {
                session.refresh().await
            } else {
                session.status().await
            };
            let text = crate::render::status_text(&status);
            Ok(text_result(text, json!({ "status": status })))
        }
        Action::ConnectAudio { device } => {
            // IOBluetooth blocks for up to 10 seconds while the link comes up.
            let message = tokio::task::spawn_blocking(move || {
                edifier_macos::connect_paired_audio(device.as_deref())
            })
            .await
            .map_err(|err| anyhow::anyhow!("audio connect task failed: {err}"))??;
            Ok(text_result(message.clone(), json!({ "message": message })))
        }
        Action::Command(command) => send_and_report(link, &command).await,
        Action::Raw(bytes) => {
            let command = Command::Raw(bytes);
            send_and_report(link, &command).await
        }
    }
}

async fn send_and_report(link: &mut DeviceLink, command: &Command) -> Result<Value> {
    let session = link.session().await?;
    let event = session
        .request(command, edifier_link::DEFAULT_TIMEOUT)
        .await?;
    let status = session.status().await;
    Ok(text_result(
        format!("{event}\n{}", status.summary()),
        json!({ "event": event.to_string(), "status": status }),
    ))
}

fn text_result(text: String, structured: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": false,
    })
}

/// Text renderers shared by the MCP tools (kept separate so they stay testable).
pub mod render {
    use edifier_proto::Status;

    /// Render a status snapshot as aligned `key value` lines.
    #[must_use]
    pub fn status_text(status: &Status) -> String {
        if status == &Status::default() {
            return "no values reported yet".to_string();
        }
        let json = serde_json::to_value(status).unwrap_or(serde_json::Value::Null);
        let Some(object) = json.as_object() else {
            return json.to_string();
        };
        object
            .iter()
            .map(|(key, value)| {
                let rendered = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                format!("{key:<16} {rendered}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initialize_echoes_the_client_protocol_version() {
        let mut link = DeviceLink::new(None);
        let request = Request::parse(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        )
        .unwrap()
        .unwrap();
        let line = handle(&mut link, &request).await.unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(value["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(
            value["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("LDAC")
        );
    }

    #[tokio::test]
    async fn tools_list_advertises_every_tool() {
        let mut link = DeviceLink::new(None);
        let request = Request::parse(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .unwrap()
            .unwrap();
        let line = handle(&mut link, &request).await.unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        let tools = value["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), tools::specs().len());
        for tool in tools {
            assert!(tool["inputSchema"]["type"] == "object");
        }
    }

    #[tokio::test]
    async fn notifications_are_not_answered() {
        let mut link = DeviceLink::new(None);
        let request = Request::parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .unwrap()
            .unwrap();
        assert!(handle(&mut link, &request).await.is_none());
    }

    #[tokio::test]
    async fn unknown_methods_and_bad_arguments_are_reported() {
        let mut link = DeviceLink::new(None);

        let request = Request::parse(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#)
            .unwrap()
            .unwrap();
        let value: Value =
            serde_json::from_str(&handle(&mut link, &request).await.unwrap()).unwrap();
        assert_eq!(value["error"]["code"], code::METHOD_NOT_FOUND);

        // Argument validation must reject before any device access, so this test needs no headset.
        let request = Request::parse(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"edifier_anc","arguments":{"mode":"loud"}}}"#,
        )
        .unwrap()
        .unwrap();
        let value: Value =
            serde_json::from_str(&handle(&mut link, &request).await.unwrap()).unwrap();
        assert_eq!(value["error"]["code"], code::INVALID_PARAMS);
        assert!(value["error"]["message"].as_str().unwrap().contains("loud"));
    }
}
