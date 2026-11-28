use crate::{
    manager::Manager,
    prelude::*,
    skills::{self, EvalAction, ReadHistory, RememberFact, SearchFact, SkillAction},
    user::{Session, UserState},
    utils,
};

use anylm::{
    api::{Content, Message, Messages, Role, Visibility},
    completions::{Chunk, Completions},
};
use osy_share::{Dialog, Event, HandleQuery, Id, UserRule};
use pearce::Callback;
use rigging::widgets::Confirmation;
use tokio::task::JoinSet;

/// Memory tool call collected while the plan is streamed (executed only after the plan is valid).
enum MemoryCall {
    Remember(RememberFact),
    Search(SearchFact),
    History(ReadHistory),
    /// The model called a tool that does not exist (it must be told, not ignored).
    Unknown(String),
}

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

                let preserve_pairs = Config::get().execution.preserve_pairs;
                let last_pairs = session.lock().await.read_last_pairs(preserve_pairs).await?;

                let mut initial_msgs = Messages::new();
                for (prev_user, prev_assistant) in last_pairs {
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
                    None,
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
///
/// Healing principles:
/// * LLM/transport errors are retried with a pause (counters protect from endless loops);
/// * errors of tools, skills and JS become tool results, so the model fixes the call itself
///   and tasks that already succeeded are never executed twice;
/// * only when the plan itself cannot be produced the user is asked whether to retry.
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
    facts_cache: Option<String>,
) -> Result<()> {
    info!("Processing the user query (iteration {iteration})...");

    let cfg = Config::get();
    let provider_options = &cfg.llm.assist_options();
    let exec_options = &cfg.execution;
    let ctx_options = &cfg.context;

    let max_iterations = exec_options.max_iterations.max(1);
    // the last round is made without tools: the model must report what it has
    let last_round = max_iterations > 1 && iteration + 1 >= max_iterations;

    // facts are searched once per user query (by the user's own words) and reused by control rounds
    let mut facts_cache = facts_cache;

    // loop repeats the CURRENT iteration in case of an error
    'iteration_loop: loop {
        // 1. Rules, Session Dialog Map & RAG facts
        let mut session_guard = session.lock().await;
        let base_system_prompt = utils::system_prompt(&session_guard.info, &cfg);

        let dialog_map_prompt = match session_guard
            .dialog_map(ctx_options.dialog_map_chars, ctx_options.dialog_map_turns)
            .await
        {
            Ok(map) => map,
            Err(e) => {
                error!("[DialogMap] Failed to build session dialog map: {e}");
                String::new()
            }
        };

        let rules_prompt = match session_guard.load_rules().await {
            Ok(rules) => build_rules_prompt(rules),
            Err(e) => {
                error!("[Rules] Failed to load session rules: {e}");
                String::new()
            }
        };
        drop(session_guard);

        if facts_cache.is_none() {
            let root_text = root_user_msg
                .as_ref()
                .unwrap_or(&message)
                .extract_texts()
                .join("\n\n");
            facts_cache = Some(load_facts_prompt(sid, &root_text).await);
        }
        let facts_prompt = facts_cache.clone().unwrap_or_default();

        // 2. Preparing the context. Stable parts go first (better for provider prompt caching):
        //    1) [system] assistant prompt + skills + rules + dialog map
        //    2) previous turn pair & steps of the current turn (WITHOUT old system blocks:
        //       they are rebuilt on every iteration, otherwise they pile up)
        //    3) [system] base prompt (OS, time) + facts
        //    4) the latest message
        let raw_messages: Vec<Message> = messages
            .lock()
            .await
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .cloned()
            .collect();

        let static_prompt = cfg
            .llm
            .assist_prompt()
            .replace("{AGENTS_LIST}", &Manager::agents_doc().await);
        let top_system = join_blocks([static_prompt, rules_prompt, dialog_map_prompt]);
        let tail_system = join_blocks([base_system_prompt, facts_prompt]);

        let mut builder = Messages::new()
            .message(Message::system(vec![top_system.into()]).visibility(Visibility::Internal));

        for m in raw_messages {
            builder = builder.message(m);
        }

        if !tail_system.trim().is_empty() {
            builder = builder.message(
                Message::system(vec![tail_system.into()]).visibility(Visibility::Internal),
            );
        }

        let messages = builder.message(message.clone()).wrap();

        info!(
            "Context prepared: {} messages",
            messages.lock().await.messages.len()
        );

        let mut agent_tasks: Vec<SkillAction> = vec![];
        let mut evals_list = vec![];
        let mut pending_memory: Vec<(String, MemoryCall)> = vec![];
        let mut final_text_response = str!();

        let mut retry_count = 0; // consecutive failures (the planning is retried in place)
        let mut total_errors = 0;
        let max_retries = exec_options.max_retries.max(1);
        let max_total_errors = exec_options.max_total_errors.max(max_retries);

        // LLM Planning cycle
        let planning_res: Result<()> = loop {
            agent_tasks.clear();
            evals_list.clear();
            pending_memory.clear();
            let mut text_response = str!();

            if tx.is_closed() {
                return Err(Error::ConnectionClosed.into());
            }

            let mut completions = Completions::try_from(provider_options.clone())?;
            if !last_round {
                completions = completions.tools(Manager::basic_tools().await);
            }

            let mut response = match completions.send(messages.clone()).await {
                Ok(res) => res,
                Err(e) => {
                    retry_count += 1;
                    total_errors += 1;
                    if retry_count < max_retries && total_errors <= max_total_errors {
                        warn!(
                            "Failed to send query completions request ({retry_count}/{max_retries}): {e}"
                        );
                        messages.lock().await.add_message(
                            Message::user(vec![
                                format!("An error occurred: {e}. Please try again to plan using the tools.").into(),
                            ])
                            .visibility(Visibility::Internal),
                        );
                        utils::backoff(retry_count).await;
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
                    Ok(Chunk::Tool(_)) if last_round => {
                        warn!("Tool call ignored: no tools on the last round");
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
                            Ok(act) => {
                                pending_memory.push((tool_call.id, MemoryCall::Remember(act)))
                            }
                            Err(e) => {
                                chunk_error =
                                    Some(format!("Failed to parse remember_fact: {e}").into());
                                break;
                            }
                        },
                        "search_fact" => match tool_call.parse_args::<SearchFact>() {
                            Ok(act) => pending_memory.push((tool_call.id, MemoryCall::Search(act))),
                            Err(e) => {
                                chunk_error =
                                    Some(format!("Failed to parse search_fact: {e}").into());
                                break;
                            }
                        },
                        "read_history" => match tool_call.parse_args::<ReadHistory>() {
                            Ok(act) => {
                                pending_memory.push((tool_call.id, MemoryCall::History(act)))
                            }
                            Err(e) => {
                                chunk_error =
                                    Some(format!("Failed to parse read_history: {e}").into());
                                break;
                            }
                        },
                        name => {
                            warn!("Unknown tool call `{name}`");
                            pending_memory
                                .push((tool_call.id.clone(), MemoryCall::Unknown(name.to_owned())));
                        }
                    },
                    Err(e) => {
                        chunk_error = Some(e.into());
                        break;
                    }
                }
            }

            if let Some(err) = chunk_error {
                retry_count += 1;
                total_errors += 1;
                if retry_count < max_retries && total_errors <= max_total_errors {
                    warn!("Stream error on planning level ({retry_count}/{max_retries}): {err}");
                    messages.lock().await.add_message(
                        Message::user(vec![
                            format!("An error occurred during stream generation: {err}. Please try again.").into(),
                        ])
                        .visibility(Visibility::Internal),
                    );
                    utils::backoff(retry_count).await;
                    continue;
                } else {
                    break Err(err);
                }
            }

            if agent_tasks.is_empty()
                && evals_list.is_empty()
                && pending_memory.is_empty()
                && text_response.trim().is_empty()
            {
                retry_count += 1;
                total_errors += 1;
                if retry_count < max_retries && total_errors <= max_total_errors {
                    let hint = if last_round {
                        "You returned an empty response. No tools are available now: write the final answer for the user in plain text."
                    } else {
                        "You returned an empty response. Execute tools or answer the user."
                    };
                    messages.lock().await.add_message(
                        Message::user(vec![hint.into()]).visibility(Visibility::Internal),
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
        // (nothing has been executed yet, so repeating the iteration cannot duplicate side effects)
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

        // 3. Memory operations (run after the plan is valid; failures become results for the model)
        let mut memory_results: Vec<(String, String, bool)> = vec![];
        for (tool_call_id, call) in pending_memory {
            let (text, silent) = run_memory_call(sid, call).await;
            memory_results.push((tool_call_id, text, silent));
        }

        let has_memory_ops = !memory_results.is_empty();
        let has_evals = !evals_list.is_empty();
        let has_tasks = !agent_tasks.is_empty();

        if has_memory_ops {
            let mut msg_guard = messages.lock().await;
            for (tool_call_id, res_text, _) in &memory_results {
                tx.send(Event::Thinking(res_text.clone()).into()).await?;
                let content_item: Content = format!("Memory Operation Result:\n{res_text}").into();
                msg_guard.push_content(Some(tool_call_id), content_item);
            }
        }

        // 4. Performing JS calculations (an error is a result: the model fixes the code)
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

                let text = match skills::handle_eval(eval).await {
                    Ok(result) => format!("JS Result:\n{result}"),
                    Err(e) => {
                        warn!("JavaScript failed, returning the error to the model: {e}");
                        tx.send(Event::Thinking(format!("JavaScript error: {e}")).into())
                            .await?;
                        format!(
                            "JS Error: {e}\nFix the code and call js_eval again, or answer without it."
                        )
                    }
                };
                results.push((tool_call_id, text));
            }

            for (tool_call_id, text) in results {
                messages
                    .lock()
                    .await
                    .push_content(Some(&tool_call_id), Content::text(text));
            }
        }

        // 5. Parallel Tool/Agent Execution
        // A failed skill writes its error into the tool result itself (see `skills::handle_skill`),
        // so here only a disconnected client or a panic can fail the workers.
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
                                error!("[handle_agent] Execution aborted: {e}");
                                workers.abort_all();
                                return Err(e);
                            }
                            Some(Err(e)) => {
                                error!("Agent task worker panicked: {e}");
                                workers.abort_all();
                                return Err(Error::Custom(e.to_string()).into());
                            }
                            None => break,
                        }
                    }
                }
            }
        }

        // 6. Recursion / Control Step Dispatch.
        // Silent memory operations (a saved fact) do not need another LLM round
        // if the model has already answered the user.
        let needs_followup = has_tasks
            || has_evals
            || memory_results.iter().any(|(_, _, silent)| !*silent)
            || (has_memory_ops && final_text_response.trim().is_empty());

        if needs_followup {
            if iteration + 1 >= max_iterations {
                warn!(
                    "Reached maximum allowed iterations ({}), stopping recursive execution.",
                    max_iterations
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

                let mut control_text = cfg.llm.control_prompt().to_owned();
                if max_iterations > 1 && iteration + 2 >= max_iterations {
                    control_text.push_str(
                        "\n\nThis is the LAST step: no tools are available anymore. \
                         Report what was done and, honestly, what failed.",
                    );
                }
                let control_msg =
                    Message::user(vec![control_text.into()]).visibility(Visibility::Internal);

                handle_query(
                    sid,
                    tx,
                    session,
                    messages,
                    control_msg,
                    root_user_msg.clone(),
                    current_path,
                    iteration + 1,
                    facts_cache,
                )
                .await?;
            }
        } else {
            // no actions left (direct final response to the user)
            tx.send(Event::Finish.into()).await?;
            info!("Query processing finished completely.");

            persist_turn_pair(&session, &messages, root_user_msg, &final_text_response).await?;
        }

        break 'iteration_loop;
    }

    Ok(())
}

/// Joins non-empty prompt blocks.
fn join_blocks<const N: usize>(blocks: [String; N]) -> String {
    blocks
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n---\n")
}

/// Builds the rules block (the model does not need IDs and scopes: they are for the user's commands).
fn build_rules_prompt(rules: Vec<UserRule>) -> String {
    if rules.is_empty() {
        return String::new();
    }

    let mut out = String::from("MANDATORY RULES & PREFERENCES (STRICTLY FOLLOW THEM):\n");
    for rule in rules {
        out.push_str(&format!("  * {}\n", rule.text.trim()));
    }
    out
}

/// Loads facts relevant to the user's message into a prompt block (empty if nothing is relevant).
///
/// The message is used as is (no translation: embeddings are multilingual); the block is limited
/// by the number of facts and by a character budget.
async fn load_facts_prompt(sid: SessionId, user_text: &str) -> String {
    if utils::is_trivial_message(user_text) {
        return String::new();
    }

    let user = match UserState::get_or_init(sid.user_id).await {
        Ok(user) => user,
        Err(e) => {
            error!("[Facts] Failed to get user state: {e}");
            return String::new();
        }
    };

    let ctx = &Config::get().context;
    let records = match user.search_facts(user_text, Some(ctx.limit())).await {
        Ok(records) => records,
        Err(e) => {
            error!("[Facts] Failed to search facts: {e}");
            return String::new();
        }
    };

    let budget = ctx.facts_max_chars;
    let mut out = String::new();
    let mut used = 0usize;
    for record in records {
        let line = format!("  * {}\n", record.data.text.trim());
        let len = line.chars().count();
        if used > 0 && used + len > budget {
            break;
        }
        used += len;
        out.push_str(&line);
    }

    if out.is_empty() {
        String::new()
    } else {
        format!("USER FACTS (relevant to this request, use when necessary):\n{out}")
    }
}

/// Executes a memory tool call. Returns the text for the model and whether the result is "silent"
/// (the model needs no further round to react to it). Errors become results, never failures.
async fn run_memory_call(sid: SessionId, call: MemoryCall) -> (String, bool) {
    if let MemoryCall::Unknown(name) = &call {
        return (
            format!(
                "Unknown tool `{name}`. Available tools: use_skill, js_eval, remember_fact, search_fact, read_history."
            ),
            false,
        );
    }

    let user = match UserState::get_or_init(sid.user_id).await {
        Ok(user) => user,
        Err(e) => return (format!("Memory is unavailable: {e}"), false),
    };

    match call {
        MemoryCall::Remember(act) => match skills::handle_remember_fact(&user, act).await {
            Ok(outcome) => (outcome.text, outcome.silent),
            Err(e) => (format!("Failed to save the fact: {e}"), false),
        },
        MemoryCall::Search(act) => match skills::handle_search_fact(&user, act).await {
            Ok(text) => (text, false),
            Err(e) => (format!("Failed to search facts: {e}"), false),
        },
        MemoryCall::History(act) => match skills::handle_read_history(&user, act).await {
            Ok(text) => (text, false),
            Err(e) => (format!("Failed to read history: {e}"), false),
        },
        MemoryCall::Unknown(_) => unreachable!("handled above"),
    }
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
