use crate::{Manager, prelude::*};

use anylm::api::{ToolCall, ToolCallFunction};
use osy_share::{Event, ToolQuery};
use pearce::Callback;

/// API: Handles skill tool call.
pub async fn handle_tool_call(
    skill_and_tool: Paths<(String, String)>,
    query: Json<ToolQuery<JsonValue>>,
) -> Response {
    let (skill_name, tool_name) = skill_and_tool.0;
    let ToolQuery {
        current_path,
        payload,
    } = query.0;

    Response::ok().stream(
        async move |tx| {
            let Some(agent) = Manager::get_by_skill(&skill_name).await else {
                let err_msg = format!("Agent for skill `{skill_name}` not found.");
                error!("[handle_tool_call] {err_msg}");
                let _ = tx.send(Event::Error(err_msg).into()).await;
                return;
            };

            let sock_path = agent.metadata.sock_path.to_string_lossy().to_string();
            let agent_name = agent.metadata.name.clone();

            let tool_call = ToolCall {
                id: "".into(),
                kind: "".into(),
                func: ToolCallFunction {
                    name: tool_name,
                    json_str: payload.to_string(),
                },
            };

            let client = Client::ipc(&sock_path);

            match handle_tool(
                client,
                sock_path,
                agent_name,
                skill_name,
                tool_call,
                current_path,
                tx.clone(),
            )
            .await
            {
                Ok((_, result_text)) => {
                    tx.send(Event::Answer(result_text).into()).await.ok();
                    tx.send(Event::Finish.into()).await.ok();
                }
                Err(e) => {
                    error!("[handle_tool_call] Execution failed: {e}");
                    tx.send(Event::Error(e.to_string()).into()).await.ok();
                }
            }
        },
        1024,
    )
}

/// Performs a single call to the agent's tool via IPC.
#[log()]
pub async fn handle_tool(
    client: Client,
    sock_path: String,
    agent_name: String,
    skill_name: String,
    tool_call: ToolCall,
    current_path: Option<PathBuf>,
    tx: Sender<Bytes>,
) -> Result<(String, String)> {
    let func = tool_call.func;
    let tool_call_id = tool_call.id;
    // a call made by the model (not a direct CLI call): its errors must reach the model
    let by_model = !tool_call_id.is_empty();
    let log_json = func.json_str.replace('\n', " ");

    let msg = format!(
        "Calling `{agent_name}.{skill_name}.{}` tool: {log_json}",
        func.name
    );
    info!("{msg}");
    tx.send(Event::Thinking(msg).into()).await.ok();

    let request_path = format!("/skills/{skill_name}/call/{}", func.name);
    let payload = match func.parse_args::<JsonValue>() {
        Ok(payload) => payload,
        // invalid arguments are the model's mistake: tell it instead of aborting the whole skill
        Err(e) if by_model => {
            return Ok((
                tool_call_id,
                format!(
                    "Tool error: the arguments are not valid JSON ({e}). Call the tool again with valid JSON arguments."
                ),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    let request_body = ToolQuery {
        current_path,
        payload,
    };
    let mut response = client
        .post(&request_path)
        .json(&request_body)
        .stream::<Event>(1024)
        .await;

    // error checking and auto-recovery of the agent if necessary
    if let Err(e) = &response {
        warn!("Agent `{agent_name}` didn't respond for tool call: {e}");
        tx.send(
            Event::Thinking(format!(
                "Agent `{agent_name}` didn't respond for tool call."
            ))
            .into(),
        )
        .await?;

        if let Some(agent) = Manager::get_agent(&agent_name).await {
            if agent.ensure().await.is_ok() {
                response = Client::ipc(&sock_path)
                    .post(&request_path)
                    .json(&request_body)
                    .stream::<Event>(1024)
                    .await;
            }
        } else {
            error!("Agent `{agent_name}` is unavailable for now.");
            return Err(
                Error::Custom(format!("Agent `{agent_name}` is unavailable for now.")).into(),
            );
        }
    }

    let mut stream = match response {
        Ok(res) => res,
        Err(e) => {
            return Err(Error::Custom(format!(
                "Agent `{agent_name}` crashed and failed to recover: {e}"
            ))
            .into());
        }
    };

    let mut full_text = str!();
    let mut errors: Vec<String> = vec![];

    while let Some(event) = tokio::select! {
        _ = tx.closed() => return Err(Error::ConnectionClosed.into()),
        res = stream.recv() => res,
    } {
        match event? {
            Event::Answer(text) => full_text.push_str(&text),
            Event::Finish => {}
            Event::Dialog(d_event) => {
                let d_event_id = d_event.get_id().to_string();
                let mut callback = Callback::register(&d_event_id);
                tx.send(Event::Dialog(d_event).into()).await?;

                let response = tokio::select! {
                    _ = tx.closed() => return Err(Error::ConnectionClosed.into()),
                    res = callback.recv::<JsonValue>(Duration::from_secs(120)) => res?,
                };

                if let Some(value) = response {
                    let client = Client::ipc(&sock_path);
                    let _ = client
                        .post(&format!("/callback/{d_event_id}"))
                        .json(&value)
                        .send()
                        .await
                        .map_err(|e| {
                            Error::Custom(format!(
                                "Failed to send callback to `{agent_name}` agent: {e}"
                            ))
                        })?;
                }
            }
            // the model must see the agent's errors to fix the call; the user gets a status line
            Event::Error(err) if by_model => {
                let _ = tx
                    .send(Event::Thinking(format!("Tool error: {err}")).into())
                    .await;
                errors.push(err);
            }
            event => {
                let _ = tx.send(event.into()).await;
            }
        }
    }

    if !errors.is_empty() {
        if !full_text.trim().is_empty() {
            full_text.push('\n');
        }
        full_text.push_str(&format!(
            "Tool error: {}\nCheck the arguments and call the tool again, or report that it cannot be done.",
            errors.join("; ")
        ));
    } else if by_model && full_text.trim().is_empty() {
        // an empty tool result confuses some providers and the model alike
        full_text = "Tool finished without output.".into();
    }

    Ok((tool_call_id, full_text))
}
