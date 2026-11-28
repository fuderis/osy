use crate::{
    Manager,
    prelude::*,
    skills::{self, EvalAction, RememberFact, SearchFact},
    user::{Session, UserState},
    utils,
};

use anylm::{
    api::{Content, Message, Messages, Tool, ToolCall},
    completions::{Chunk, Completions},
};
use osy_share::{Event, HandleQuery, SessionId, SessionInfo};
use std::collections::HashMap;
use tokio::task::JoinSet;

/// API: Handles skill query.
pub async fn handle_skill_query(
    paths: Paths<(SessionId, String)>,
    query: Json<HandleQuery>,
) -> Response {
    let (sid, skill_name) = paths.0;
    let HandleQuery {
        current_path,
        message,
    } = query.0;

    Response::ok().stream(move |tx| async move {
        let result = match Session::read(sid).await {
            Ok((session, _messages)) => {
                let session_info = session.lock().await.info.clone();

                match handle_skill(
                    tx.clone(),
                    session_info,
                    &skill_name,
                    message.clone(),
                    current_path,
                    true,
                )
                .await
                {
                    Ok(result_text) => {
                        let assistant_msg = Message::assistant(vec![result_text.clone().into()], vec![]);
                        let pair = (message, assistant_msg);
                        if let Err(e) = session
                            .lock()
                            .await
                            .write_messages(pair)
                            .await
                        {
                            error!("[handle_skill_query{{sid={sid}}}] Failed to write turn pair: {e}");
                            Err(e)
                        } else {
                            let _ = tx.send(Event::Finish.into()).await;
                            Ok(())
                        }
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };

        if let Err(e) = result {
            error!("[handle_skill_query{{sid={sid}}}] Execution failed: {e}");
            tx.send(Event::Error(e.to_string()).into()).await.ok();
        }
    }, 1024)
}

/// Performs agent's skill (creates the context, launches the LLM cycle, self-healing and distribution of sub-tools).
#[log()]
pub async fn handle_skill(
    tx: Sender<Bytes>,
    session_info: SessionInfo,
    skill_name: &str,
    message: Message,
    current_path: Option<PathBuf>,
    stream_answer: bool,
) -> Result<String> {
    let Some(agent) = Manager::get_by_skill(skill_name).await else {
        return Err(
            Error::Custom(format!("Using unknown skill `{skill_name}`, aborting...")).into(),
        );
    };

    // extracting agent metadata
    let sock_path = agent.metadata.sock_path.to_string_lossy().to_string();
    let agent_name = agent.metadata.name.clone();
    let Some(skill) = agent.metadata.skills.get(skill_name) else {
        return Err(Error::Custom(format!(
            "Failed to get `{skill_name}` skill metadata, aborting..."
        ))
        .into());
    };
    let agent_skill_prompt = skill.prompt.trim().to_owned();

    // loading agent's list of tools and combining with skill basic tools
    let client = Client::ipc(&sock_path);
    let mut tools = Manager::skill_basic_tools().await;
    let agent_tools = client
        .post(&format!("/skills/{skill_name}/tools"))
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| {
            Error::Custom(format!(
                "Failed to get the `{agent_name}` agent tools list: {e}"
            ))
        })?
        .json::<Vec<Tool>>()
        .await?;
    tools.extend(agent_tools);

    let msg = format!(
        "Handling `{agent_name}.{skill_name}` skill: \"{}...\"",
        message.extract_texts()[0]
            .chars()
            .take(40)
            .collect::<String>()
            .trim_end_matches('.')
            .replace('\n', " ")
    );
    info!("{msg}");
    tx.send(Event::Thinking(msg).into()).await.ok();

    let cfg = Config::get();
    let provider_options = cfg.llm.skill_options();
    let exec_options = &cfg.execution;

    // assembling context messages with config skill prompt & agent skill prompt
    let agent_messages = {
        let mut msgs = Messages::new();
        let mut system_content = vec![utils::system_prompt(&session_info, &cfg).into()];

        let config_skill_prompt = cfg.llm.skill_prompt();
        if !config_skill_prompt.is_empty() {
            system_content.push(config_skill_prompt.to_string().into());
        }

        if !agent_skill_prompt.is_empty() {
            system_content.push(agent_skill_prompt.into());
        }

        msgs.add_system(system_content);
        msgs.add_system(vec![
            "For the following request, you MUST use the provided tools and MUST NOT answer from your own knowledge. \
             If no suitable tool is available, return an error explaining that the required tool does not exist. \
             Never invent or assume tools that were not provided.".into()
        ]);
        msgs.add_message(message);

        msgs.wrap()
    };

    let max_retries = exec_options.max_retries.max(1);
    let max_steps = exec_options.skill_max_iterations.max(1);
    let max_total_errors = exec_options.max_total_errors.max(max_retries);
    let max_identical = exec_options.max_identical_calls.max(1);

    let mut retry_count = 0; // consecutive failures (reset after every successful step)
    let mut total_errors = 0; // all failures of this run
    let mut steps = 0; // successful LLM steps
    let mut final_step_announced = false;
    let mut seen_calls: HashMap<String, usize> = HashMap::new();

    // cycle of generation and self-healing:
    // * transport/LLM errors are retried with a pause (the counters protect from endless loops);
    // * tool errors are returned to the model as results, so it fixes the call on the spot.
    loop {
        if tx.is_closed() {
            return Err(Error::ConnectionClosed.into());
        }

        if steps >= max_steps {
            warn!(
                "Agent `{agent_name}` reached maximum allowed steps ({max_steps}) for skill `{skill_name}`."
            );
            return Err(Error::Custom(format!(
                "Agent `{agent_name}` exceeded maximum execution limit of {max_steps} steps."
            ))
            .into());
        }

        // the last step is made without tools: the model reports what was done and what failed
        let last_step = max_steps > 1 && steps + 1 >= max_steps;
        if last_step && !final_step_announced {
            final_step_announced = true;
            agent_messages.lock().await.add_user(vec![
                "Step limit reached. Do not call tools anymore: report the result so far and clearly state what failed or is not done.".into(),
            ]);
        }

        let mut tool_calls = vec![];
        let mut text_response = str!();

        let mut completions = Completions::try_from(provider_options.clone())?;
        if !last_step {
            completions = completions.tools(tools.clone());
        }

        match completions.send(agent_messages.clone()).await {
            Ok(mut stream) => {
                let mut chunk_error = None;

                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(Chunk::Text(text_part)) => {
                            if stream_answer {
                                if let Err(e) =
                                    tx.send(Event::Answer(text_part.clone()).into()).await
                                {
                                    warn!("[handle_skill] Failed to stream chunk to client: {e}");
                                }
                            }
                            text_response.push_str(&text_part);
                        }
                        Ok(Chunk::Tool(tool_call)) => {
                            tool_calls.push(tool_call);
                        }
                        Err(e) => {
                            chunk_error = Some(e);
                            break;
                        }
                    }
                }

                if let Some(err) = chunk_error {
                    retry_count += 1;
                    total_errors += 1;
                    if retry_count < max_retries && total_errors <= max_total_errors {
                        warn!(
                            "Error reading stream from agent `{agent_name}`. Retrying ({retry_count}/{max_retries}): {err}"
                        );
                        tx.send(
                            Event::Thinking(format!(
                                "Stream error. Retrying {agent_name} agent execution..."
                            ))
                            .into(),
                        )
                        .await?;

                        agent_messages.lock().await.add_user(vec![
                            format!("An error occurred during output generation: {err}. Please try again using tools.").into()
                        ]);

                        utils::backoff(retry_count).await;
                        continue;
                    } else {
                        return Err(Error::Custom(format!(
                            "Agent `{agent_name}` failed after stream error: {err}"
                        ))
                        .into());
                    }
                }

                if tool_calls.is_empty() && text_response.trim().is_empty() {
                    retry_count += 1;
                    total_errors += 1;
                    if retry_count < max_retries && total_errors <= max_total_errors {
                        warn!(
                            "Agent `{agent_name}` returned empty response and no tool calls. Retrying ({retry_count}/{max_retries})..."
                        );
                        tx.send(
                            Event::Thinking(format!(
                                "Agent {agent_name} returned empty response. Retrying task..."
                            ))
                            .into(),
                        )
                        .await?;

                        agent_messages.lock().await.add_user(vec![
                            "You did not call any tools. Please execute the requested task using tools now.".into()
                        ]);

                        continue;
                    } else {
                        return Err(Error::Custom(format!(
                            "Agent `{agent_name}` failed after {max_retries} retries: empty output"
                        ))
                        .into());
                    }
                }
            }
            Err(e) => {
                retry_count += 1;
                total_errors += 1;
                if retry_count < max_retries && total_errors <= max_total_errors {
                    warn!(
                        "Failed to send completions request for agent `{agent_name}` ({retry_count}/{max_retries}): {e}"
                    );
                    tx.send(
                        Event::Thinking(format!(
                            "Request error. Retrying {agent_name} agent execution..."
                        ))
                        .into(),
                    )
                    .await?;

                    agent_messages.lock().await.add_user(vec![
                        format!("Failed to process request due to error: {e}. Please attempt to execute the task again.").into()
                    ]);

                    utils::backoff(retry_count).await;
                    continue;
                } else {
                    return Err(Error::Custom(format!(
                        "Agent `{agent_name}` failed sending completions request: {e}"
                    ))
                    .into());
                }
            }
        }

        if tx.is_closed() {
            return Err(Error::ConnectionClosed.into());
        }

        // a valid step: the consecutive failure counter starts over
        steps += 1;
        retry_count = 0;

        // if no tool calls (or tools were not offered on the last step), return the final response of the skill
        if tool_calls.is_empty() || last_step {
            return Ok(format!("Agent `{agent_name}` response:\n{text_response}"));
        }

        let mut sub_workers = JoinSet::new();
        let mut blocked: Vec<(String, String)> = vec![];

        // parallel execution of sub-tool calls (handling both basic system tools and IPC tools)
        for tool_call in tool_calls {
            // protection from endless loops: the same call with the same arguments is not executed again and again
            let key = format!("{}:{}", tool_call.func.name, tool_call.func.json_str);
            let times = seen_calls.entry(key).or_insert(0);
            *times += 1;
            if *times > max_identical {
                blocked.push((
                    tool_call.id.clone(),
                    format!(
                        "Tool error: this exact call was already made {max_identical} times. \
                         Use the earlier results, change the arguments, or report that the task cannot be completed."
                    ),
                ));
                continue;
            }

            let client = client.clone();
            let sock_path = sock_path.clone();
            let agent_name = agent_name.clone();
            let tx = tx.clone();
            let skill_name = skill_name.to_string();
            let session_info = session_info.clone();
            let current_path = current_path.clone();

            sub_workers.spawn(
                async move {
                    let call_id = tool_call.id.clone();

                    match run_sub_tool(
                        client,
                        sock_path,
                        agent_name,
                        skill_name,
                        session_info,
                        current_path,
                        tx.clone(),
                        tool_call,
                    )
                    .await
                    {
                        Ok(res) => Ok(res),
                        // the client has gone: nothing to heal
                        Err(e) if tx.is_closed() => Err(e),
                        // otherwise the error becomes the tool result, so the model can fix the call
                        Err(e) => {
                            warn!("Sub-tool failed, returning the error to the model: {e}");
                            Ok((
                                call_id,
                                format!(
                                    "Tool error: {e}\nCheck the arguments and call the tool again, or report that it cannot be done."
                                ),
                            ))
                        }
                    }
                }
                .log_span(Span::current()),
            );
        }

        if !blocked.is_empty() {
            let mut guard = agent_messages.lock().await;
            for (tool_call_id, text) in blocked {
                guard.push_content(
                    Some(&tool_call_id),
                    Content::text(format!("Tool execution result:\n{text}")),
                );
            }
        }

        // collecting results of work of the sub-tools
        loop {
            tokio::select! {
                _ = tx.closed() => {
                    sub_workers.abort_all();
                    return Err(Error::ConnectionClosed.into());
                }
                maybe_res = sub_workers.join_next() => {
                    match maybe_res {
                        Some(Ok(Ok((tool_call_id, result_text)))) => {
                            let mut guard = agent_messages.lock().await;
                            guard.push_content(
                                Some(&tool_call_id),
                                Content::text(format!("Tool execution result:\n{result_text}")),
                            );
                        }
                        Some(Ok(Err(e))) => {
                            error!("Sub-tool execution aborted: {e}");
                            sub_workers.abort_all();
                            return Err(e);
                        }
                        Some(Err(e)) => {
                            error!("Sub-tool worker panicked: {e}");
                            sub_workers.abort_all();
                            return Err(e.into());
                        }
                        None => break,
                    }
                }
            }
        }
    }
}

/// Executes one tool call of the skill (basic runtime tools or an agent tool over IPC).
///
/// Errors are returned to the caller, which turns them into a tool result for the model.
async fn run_sub_tool(
    client: Client,
    sock_path: String,
    agent_name: String,
    skill_name: String,
    session_info: SessionInfo,
    current_path: Option<PathBuf>,
    tx: Sender<Bytes>,
    tool_call: ToolCall,
) -> Result<(String, String)> {
    match tool_call.func.name.as_str() {
        "js_eval" => {
            let eval: EvalAction = tool_call.parse_args()?;
            tx.send(
                Event::Thinking(format!(
                    "Executing JavaScript code: {:80}...",
                    &eval.code
                ))
                .into(),
            )
            .await
            .ok();
            let result = skills::handle_eval(eval).await?;
            Ok((tool_call.id, format!("JS Result:\n{result}")))
        }
        "remember_fact" => {
            let act: RememberFact = tool_call.parse_args()?;
            let user = UserState::get_or_init(session_info.id.user_id).await?;
            let res = skills::handle_remember_fact(&user, act).await?;
            tx.send(Event::Thinking(res.text.clone()).into()).await.ok();
            Ok((tool_call.id, format!("Memory Operation Result:\n{}", res.text)))
        }
        "search_fact" => {
            let act: SearchFact = tool_call.parse_args()?;
            let user = UserState::get_or_init(session_info.id.user_id).await?;
            let res_msg = skills::handle_search_fact(&user, act).await?;
            tx.send(Event::Thinking(res_msg.clone()).into()).await.ok();
            Ok((tool_call.id, format!("Memory Operation Result:\n{res_msg}")))
        }
        _ => {
            super::handle_tool(
                client,
                sock_path,
                agent_name,
                skill_name,
                tool_call,
                current_path,
                tx,
            )
            .await
        }
    }
}
