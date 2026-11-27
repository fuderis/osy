use crate::{
    manager::Manager,
    prelude::*,
    skills::{self, EvalAction, ReadHistory, RememberFact, SearchFact, SkillAction},
    user::{Session, UserState},
    utils,
};

use anylm::{
    api::{Content, Message, Messages, Visibility},
    completions::{Chunk, Completions},
};
use osy_share::{Dialog, Event, HandleQuery, Id};
use pearce::Callback;
use rigging::widgets::Confirmation;
use tokio::task::JoinSet;

const DIALOG_MAP_MAX_CHARS: usize = 160;

/// API: User query handler.
pub async fn handle_user_query(paths: Paths<SessionId>, query: Json<HandleQuery>) -> Response {
    let sid = paths.0;
    let HandleQuery {
        current_path,
        message,
    } = query.0;

    Response::ok().stream(
        move |tx| async move {
            let result = async {
                let session = Session::require(&sid)?;
                let last_pair = session.lock().await.read_last_pair().await?;

                let mut initial_msgs = Messages::new();
                if let Some((prev_user, prev_assistant)) = last_pair {
                    initial_msgs.add_message(prev_user);
                    initial_msgs.add_message(prev_assistant);
                }
                let messages = Arc::new(Mutex::new(initial_msgs));

                handle_query(
                    sid,
                    tx.clone(),
                    session,
                    messages,
                    message.clone(),
                    Some(message),
                    current_path,
                    0,
                )
                .await
            }
            .await;

            if let Err(e) = result {
                error!("[handle_query{{sid={sid}}}] {e}");
                tx.send(Event::Error(e.to_string()).into()).await.ok();
            }
        },
        1024,
    )
}

/// Handles user query with direct parallel tool execution and self-healing.
#[async_recursion]
#[log(sid = %sid)]
async fn handle_query(
    sid: SessionId,
    tx: Sender<Bytes>,
    session: Arc<Mutex<Session>>,
    messages: Arc<Mutex<Messages>>,
    message: Message,
    root_user_msg: Option<Message>,
    current_path: Option<PathBuf>,
    iteration: usize,
) -> Result<()> {
    info!("Processing the user query (iteration {iteration})...");

    let cfg = Config::get();
    let provider_options = &cfg.llm.assist_options();
    let assist_prompt = &cfg.llm.assist_prompt();
    let exec_options = &cfg.execution;

    // loop repeats the CURRENT iteration in case of an error
    'iteration_loop: loop {
        // 1. RAG, Rules & Session Dialog Map
        let session_guard = session.lock().await;
        let base_system_prompt = utils::system_prompt(&session_guard.info, &cfg);

        let dialog_map_prompt = match session_guard.dialog_map(DIALOG_MAP_MAX_CHARS).await {
            Ok(map) => map,
            Err(e) => {
                error!("[DialogMap] Failed to build session dialog map: {e}");
                String::new()
            }
        };

        let mut facts_prompt = String::new();
        let mut rules_prompt = String::new();

        match session_guard.load_rules().await {
            Ok(rules) => {
                if !rules.is_empty() {
                    rules_prompt
                        .push_str("MANDATORY RULES & PREFERENCES (STRICTLY FOLLOW THEM):\n");
                    for rule in rules {
                        let scope = if rule.is_global { "Global" } else { "Local" };
                        rules_prompt.push_str(&format!(
                            "  * [ID: {}, Scope: {}] {}\n",
                            rule.id, scope, rule.text
                        ));
                    }
                }
            }
            Err(e) => error!("[Rules] Failed to load session rules: {e}"),
        }
        drop(session_guard);

        let user_text = message.extract_texts().join("\n\n");
        if !user_text.trim().is_empty() {
            match {
                let user = UserState::get_or_init(sid.user_id).await?;
                let limit = Config::get().context.search_limit;
                user.search_facts(&user_text, Some(limit)).await
            } {
                Ok(facts) if !facts.is_empty() => {
                    facts_prompt.push_str("LOADED USER FACTS (USE THEM WHEN NECESSARY):\n");
                    for record in facts {
                        facts_prompt
                            .push_str(&format!("  * [ID: {}] {}\n", record.id, record.data.text));
                    }
                }
                Err(e) => error!("[Facts] Failed to search facts in LanceDB: {e}"),
                _ => {}
            }
        }

        // 2. Preparing the context:
        //    1) Dialog map at the very top as a system anchor
        //    2) Previous turn pair & current turn steps from `raw_messages`
        //    3) System prompt (base + rules + facts + skills) before the latest message
        let raw_messages = messages.lock().await.messages.clone();

        let system_body = [base_system_prompt, rules_prompt, facts_prompt]
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n---\n");

        let mut builder = Messages::new();

        if !dialog_map_prompt.trim().is_empty() {
            builder = builder.message(
                Message::system(vec![dialog_map_prompt.into()]).visibility(Visibility::Internal),
            );
        }

        for m in raw_messages {
            builder = builder.message(m);
        }

        builder = builder.message(
            Message::system(vec![
                system_body.into(),
                assist_prompt
                    .replace("{AGENTS_LIST}", &Manager::agents_doc().await)
                    .into(),
            ])
            .visibility(Visibility::Internal),
        );

        let messages = builder.message(message.clone()).wrap();

        warn!("CONTEXT MESSAGES: {:#?}", messages.lock().await); // DEBUG

        let mut agent_tasks = vec![];
        let mut evals_list = vec![];
        let mut memory_results = vec![];
        let mut final_text_response = str!();

        let mut retry_count = 0;
        let max_retries = exec_options.max_retries.max(1);

        // LLM Planning cycle
        let planning_res: Result<()> = loop {
            agent_tasks.clear();
            evals_list.clear();
            memory_results.clear();
            let mut text_response = str!();

            if tx.is_closed() {
                return Err(Error::ConnectionClosed.into());
            }

            let mut response = match Completions::try_from(provider_options.clone())?
                .tools(Manager::basic_tools().await)
                .send(messages.clone())
                .await
            {
                Ok(res) => res,
                Err(e) => {
                    retry_count += 1;
                    if retry_count < max_retries {
                        warn!(
                            "Failed to send query completions request ({retry_count}/{max_retries}): {e}"
                        );
                        messages.lock().await.add_message(
                            Message::user(vec![
                                format!("An error occurred: {e}. Please try again to plan using the tools.").into(),
                            ])
                            .visibility(Visibility::Internal),
                        );
                        continue;
                    } else {
                        break Err(e.into());
                    }
                }
            };

            let mut chunk_error = None;
            while let Some(chunk) = tokio::select! {
                _ = tx.closed() => return Err(Error::ConnectionClosed.into()),
                chunk = response.next() => chunk,
            } {
                match chunk {
                    Ok(Chunk::Text(text_part)) => {
                        text_response.push_str(&text_part);
                        tx.send(Event::Answer(text_part).into()).await?;
                    }
                    Ok(Chunk::Tool(tool_call)) => match tool_call.func.name.as_ref() {
                        "use_skill" => match tool_call.parse_args::<SkillAction>() {
                            Ok(mut task) => {
                                task.tool_call_id = tool_call.id;
                                agent_tasks.push(task);
                            }
                            Err(e) => {
                                chunk_error =
                                    Some(format!("Failed to parse handle_task: {e}").into());
                                break;
                            }
                        },
                        "js_eval" => match tool_call.parse_args::<EvalAction>() {
                            Ok(eval) => evals_list.push((tool_call.id, eval)),
                            Err(e) => {
                                chunk_error = Some(format!("Failed to parse js_eval: {e}").into());
                                break;
                            }
                        },
                        "remember_fact" => match tool_call.parse_args::<RememberFact>() {
                            Ok(act) => match {
                                let user = UserState::get_or_init(sid.user_id).await?;
                                skills::handle_remember_fact(&user, act).await
                            } {
                                Ok(res_msg) => memory_results.push((tool_call.id, res_msg)),
                                Err(e) => {
                                    chunk_error = Some(format!("Failed to save fact: {e}").into());
                                    break;
                                }
                            },
                            Err(e) => {
                                chunk_error =
                                    Some(format!("Failed to parse remember_fact: {e}").into());
                                break;
                            }
                        },
                        "search_fact" => match tool_call.parse_args::<SearchFact>() {
                            Ok(act) => match {
                                let user = UserState::get_or_init(sid.user_id).await?;
                                skills::handle_search_fact(&user, act).await
                            } {
                                Ok(res_msg) => memory_results.push((tool_call.id, res_msg)),
                                Err(e) => {
                                    chunk_error =
                                        Some(format!("Failed to search facts: {e}").into());
                                    break;
                                }
                            },
                            Err(e) => {
                                chunk_error =
                                    Some(format!("Failed to parse search_fact: {e}").into());
                                break;
                            }
                        },
                        "read_history" => match tool_call.parse_args::<ReadHistory>() {
                            Ok(act) => match {
                                let user = UserState::get_or_init(sid.user_id).await?;
                                skills::handle_read_history(&user, act).await
                            } {
                                Ok(res_msg) => memory_results.push((tool_call.id, res_msg)),
                                Err(e) => {
                                    chunk_error =
                                        Some(format!("Failed to read history: {e}").into());
                                    break;
                                }
                            },
                            Err(e) => {
                                chunk_error =
                                    Some(format!("Failed to parse read_history: {e}").into());
                                break;
                            }
                        },
                        name => warn!("Unknown tool call `{name}`"),
                    },
                    Err(e) => {
                        chunk_error = Some(e.into());
                        break;
                    }
                }
            }

            if let Some(err) = chunk_error {
                retry_count += 1;
                if retry_count < max_retries {
                    warn!("Stream error on planning level ({retry_count}/{max_retries}): {err}");
                    messages.lock().await.add_message(
                        Message::user(vec![
                            format!("An error occurred during stream generation: {err}. Please try again.").into(),
                        ])
                        .visibility(Visibility::Internal),
                    );
                    continue;
                } else {
                    break Err(err);
                }
            }

            if agent_tasks.is_empty()
                && evals_list.is_empty()
                && memory_results.is_empty()
                && text_response.trim().is_empty()
            {
                retry_count += 1;
                if retry_count < max_retries {
                    messages.lock().await.add_message(
                        Message::user(vec![
                            "You returned an empty response. Execute tools or answer the user."
                                .into(),
                        ])
                        .visibility(Visibility::Internal),
                    );
                    continue;
                } else {
                    break Err(
                        format!("Model failed to plan tasks: returned empty response").into(),
                    );
                }
            }

            final_text_response = text_response;
            break Ok(());
        };

        // if planning has failed after all the retreats
        if let Err(e) = planning_res {
            tx.send(Event::Error(e.to_string()).into()).await.ok();
            let prompt =
                format!("The planning failed with an error: {e}. Do you want to try again?");
            if ask_retry(&tx, prompt).await.unwrap_or(false) {
                info!(
                    "User requested retry after planning error. Retrying iteration {iteration}..."
                );
                continue 'iteration_loop;
            } else {
                return Err(e);
            }
        }

        let has_memory_ops = !memory_results.is_empty();
        let has_evals = !evals_list.is_empty();
        let has_tasks = !agent_tasks.is_empty();

        if has_memory_ops || has_evals || has_tasks {
            let mut guard = messages.lock().await;
            if iteration == 0 {
                guard.add_message(message.clone());
            }
        }

        // 3. Process Memory Operations & Notifications
        if has_memory_ops {
            let mut msg_guard = messages.lock().await;
            for (tool_call_id, res_text) in memory_results {
                tx.send(Event::Thinking(res_text.clone()).into()).await?;
                let content_item: Content = format!("Memory Operation Result:\n{res_text}").into();
                msg_guard.push_content(Some(&tool_call_id), content_item);
            }
        }

        // 4. Performing JS calculations
        if has_evals {
            let mut results: Vec<(String, String)> = vec![];
            for (tool_call_id, eval) in evals_list {
                tx.send(
                    Event::Thinking(
                        format!("Executing JavaScript code: {:80}...", &eval.code).into(),
                    )
                    .into(),
                )
                .await?;
                let result = skills::handle_eval(eval).await?;
                results.push((tool_call_id, format!("JS Result:\n{result}").into()));
            }

            for (tool_call_id, content_item) in results {
                messages
                    .lock()
                    .await
                    .push_content(Some(&tool_call_id), content_item);
            }
        }

        // 5. Parallel Tool/Agent Execution
        let mut execution_error = None;
        if has_tasks {
            info!("Executing {} agent tasks in parallel", agent_tasks.len());
            let mut workers = JoinSet::new();

            for task in agent_tasks {
                let session = session.clone();
                let messages = messages.clone();
                let current_path = current_path.clone();
                let tx = tx.clone();

                workers.spawn(
                    async move {
                        skills::handle_skill(
                            tx,
                            session.lock().await.info.clone(),
                            messages,
                            current_path,
                            task,
                        )
                        .await
                    }
                    .log_span(Span::current()),
                );
            }

            loop {
                tokio::select! {
                    _ = tx.closed() => {
                        warn!("Client disconnected, aborting agent execution");
                        workers.abort_all();
                        return Err(Error::ConnectionClosed.into());
                    }
                    maybe_res = workers.join_next() => {
                        match maybe_res {
                            Some(Ok(Ok(_))) => {}
                            Some(Ok(Err(e))) => {
                                error!("[handle_agent] Execution failed: {e}");
                                execution_error = Some(e);
                            }
                            Some(Err(e)) => {
                                error!("Agent task worker panicked: {e}");
                                execution_error = Some(Error::Custom(e.to_string()).into());
                            }
                            None => break,
                        }
                    }
                }
            }
        }

        // if task execution has dropped after max_retries inside handle_task
        if let Some(err) = execution_error {
            tx.send(Event::Error(err.to_string()).into()).await.ok();
            let prompt =
                format!("Task processing failed with an error: {err}. Do you want to try again?");
            if ask_retry(&tx, prompt).await.unwrap_or(false) {
                info!("User requested retry after task error. Retrying iteration {iteration}...");
                continue 'iteration_loop;
            } else {
                return Err(err);
            }
        }

        // 6. Recursion / Control Step Dispatch
        let had_actions = has_tasks || has_evals || has_memory_ops;

        if had_actions {
            if iteration + 1 >= exec_options.max_iterations {
                warn!(
                    "Reached maximum allowed iterations ({}), stopping recursive execution.",
                    exec_options.max_iterations
                );
                tx.send(Event::Finish.into()).await?;

                persist_turn_pair(
                    &session,
                    &messages,
                    root_user_msg.clone(),
                    &final_text_response,
                )
                .await?;
            } else {
                info!(
                    "Sub-tasks finished. Launching control query (iteration {})...",
                    iteration + 1
                );
                let control_msg = Message::user(vec![cfg.llm.control_prompt().into()])
                    .visibility(Visibility::Internal);

                handle_query(
                    sid,
                    tx,
                    session,
                    messages,
                    control_msg,
                    root_user_msg.clone(),
                    current_path,
                    iteration + 1,
                )
                .await?;
            }
        } else {
            // no actions (direct final response to the user)
            tx.send(Event::Finish.into()).await?;
            info!("Query processing finished completely.");

            persist_turn_pair(&session, &messages, root_user_msg, &final_text_response).await?;
        }

        break 'iteration_loop;
    }

    Ok(())
}

/// Persists `(User, Assistant)` pair when a top-level turn finishes.
async fn persist_turn_pair(
    session: &Arc<Mutex<Session>>,
    messages: &Arc<Mutex<Messages>>,
    root_user_msg: Option<Message>,
    final_text_response: &str,
) -> Result<()> {
    let Some(user_msg) = root_user_msg else {
        return Ok(());
    };

    let assistant_msg = if !final_text_response.trim().is_empty() {
        Message::assistant(vec![final_text_response.to_owned().into()], vec![])
    } else {
        messages
            .lock()
            .await
            .messages
            .iter()
            .rev()
            .find(|m| m.role.is_assistant())
            .cloned()
            .unwrap_or_else(|| Message::assistant(vec!["".into()], vec![]))
    };

    let pair = (user_msg, assistant_msg);
    session.lock().await.write_messages(pair).await?;

    Ok(())
}

/// Requesting confirmation from the user to try again in case of an error.
async fn ask_retry(tx: &Sender<Bytes>, prompt: String) -> Result<bool> {
    let id = Id::new().to_string();
    let mut callback = Callback::register(&id);

    tx.send(
        Event::Dialog(Dialog::Confirm {
            id,
            prompt,
            default: Some(Confirmation::Yes),
        })
        .into(),
    )
    .await?;

    let response = tokio::select! {
        _ = tx.closed() => return Err(Error::ConnectionClosed.into()),
        res = callback.recv::<Confirmation>(Duration::from_secs(300)) => res,
    };

    Ok(match response.unwrap_or(None) {
        Some(Confirmation::Yes) | Some(Confirmation::Always) => true,
        _ => false,
    })
}
