use crate::{prelude::*, user::Session};

use osy_share::{RemoveQuery, RenameQuery, SessionId, SessionInfo, SetQuery};

/// Returns the active session or early-returns an error response.
macro_rules! session {
    ($sid:expr) => {
        match Session::require(&$sid) {
            Ok(session) => session,
            Err(e) => {
                error!("{e}");
                return Response::error().text(e.to_string());
            }
        }
    };
}

/// API: Returns session metadata by ID.
#[log(sid = %sid)]
pub async fn handle_session_metadata(Paths(sid): Paths<SessionId>) -> Response {
    info!("Fetching metadata for session `{sid}`...");

    let session_shared = session!(sid);

    info!("Waiting for session lock...");
    let metadata = {
        let session = session_shared.lock().await;
        session.metadata.clone()
    };

    Response::ok().json(&metadata)
}

/// API: Initializes user session and returns its messages.
#[log(sid = %sid)]
pub async fn handle_session_init(
    Paths(sid): Paths<SessionId>,
    query: Json<SessionInfo>,
) -> Response {
    let session_info = query.0;
    info!("Handling session init/get...");

    // get active session, or initialize a new one
    let session_shared = match Session::get_or_init(sid, session_info).await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to init session {sid}: {e}");
            return Response::error().text(e.to_string());
        }
    };

    info!("Waiting for session lock...");
    let read_result = {
        let session = session_shared.lock().await;
        let res = session.read_messages().await;
        info!("Messages read. Releasing session lock...");
        res
    };

    match read_result {
        Ok(messages) => Response::ok().json(&messages),
        Err(e) => {
            error!("Failed to read messages for session {sid}: {e}");
            Response::error().text(e.to_string())
        }
    }
}

/// API: Finishes user session and flushes DB to prevent lock contention.
#[log(sid = %sid)]
pub async fn handle_session_finish(Paths(sid): Paths<SessionId>) -> Response {
    match Session::finish(&sid).await {
        Ok(_) => Response::ok().text("Session finished successfully"),
        Err(e) => {
            error!("Failed to finish session {sid}: {e}");
            Response::error().text(e.to_string())
        }
    }
}

/// API: Renames active user session.
#[log(sid = %sid)]
pub async fn handle_session_rename(
    Paths(sid): Paths<SessionId>,
    query: Json<RenameQuery>,
) -> Response {
    let RenameQuery { name } = query.0;
    info!("Renaming session `{sid}` to `{name}`...");

    let session_shared = session!(sid);

    info!("Waiting for session lock...");
    let rename_res = {
        let mut session = session_shared.lock().await;
        info!("Acquired session lock. Renaming session...");
        let res = session.rename(name).await;
        info!("Rename operation completed. Releasing session lock...");
        res
    };

    match rename_res {
        Ok(_) => {
            info!("Session `{sid}` renamed successfully.");
            Response::ok().text("Session renamed successfully")
        }
        Err(e) => {
            error!("Failed to rename session `{sid}`: {e}");
            Response::error().text(e.to_string())
        }
    }
}

/// Completely clears the session message history
#[log(sid = %sid)]
pub async fn handle_session_clear(Paths(sid): Paths<SessionId>) -> Response {
    if let Some(session_shared) = Session::get(&sid) {
        info!("Waiting for session lock...");
        let res = session_shared.lock().await.clear().await;

        if let Err(e) = res {
            error!("Failed to clear session {sid}: {e}");
            return Response::error().text(e.to_string());
        }
    } else {
        warn!("Attempted to clear non-existent session {sid}.");
    }

    Response::ok()
}

/// API: Completely removes user session from memory and disk.
#[log(sid = %sid)]
pub async fn handle_session_remove(Paths(sid): Paths<SessionId>) -> Response {
    info!("Initiating full removal for session `{sid}`...");

    match Session::remove(&sid).await {
        Ok(_) => {
            info!("Session `{sid}` removed successfully.");
            Response::ok().text("Session removed successfully")
        }
        Err(e) => {
            error!("Failed to remove session `{sid}`: {e}");
            Response::error().text(e.to_string())
        }
    }
}

/// Clones user session and returns a new ID.
#[log(sid = %sid)]
pub async fn handle_session_clone(Paths(sid): Paths<SessionId>) -> Response {
    if let Some(session_shared) = Session::get(&sid) {
        info!("Waiting for session lock...");
        let clone_res = {
            let session = session_shared.lock().await;
            let res = session.duplicate().await;
            info!("Duplicate complete. Releasing lock...");
            res
        };

        match clone_res {
            Ok(new_sid) => Response::ok().json(&json!({ "id": new_sid })),
            Err(e) => {
                let msg = format!("Failed to clone session: {e}");
                error!("{msg}");
                Response::error().text(msg)
            }
        }
    } else {
        let msg = format!("Session `{sid}` is not defined");
        error!("{msg}");
        Response::error().text(msg)
    }
}

// --- LOCAL SESSION RULES HANDLERS ---

/// API: Lists active rules (global + local) for a session.
#[log(sid = %sid)]
pub async fn handle_session_rules_list(Paths(sid): Paths<SessionId>) -> Response {
    let session_shared = session!(sid);

    info!("Waiting for session lock...");
    let rules_res = {
        let session = session_shared.lock().await;
        info!("Acquired session lock. Listing session rules...");
        let res = session.load_rules().await;
        info!("Rules listed. Releasing lock...");
        res
    };

    match rules_res {
        Ok(rules) => {
            info!("Finished listing rules for session {sid}");
            Response::ok().json(&rules)
        }
        Err(e) => {
            error!("Failed to list rules for session {sid}: {e}");
            Response::error().text(e.to_string())
        }
    }
}

/// API: Adds or updates rule in the session or global context.
#[log(sid = %sid)]
pub async fn handle_session_rules_set(
    Paths(sid): Paths<SessionId>,
    query: Json<SetQuery>,
) -> Response {
    let SetQuery { id, text } = query.0;
    info!("Setting rule (id: {id:?})...");

    let session_shared = session!(sid);

    info!("Waiting for session lock...");
    let save_res = {
        let session = session_shared.lock().await;
        info!("Acquired session lock...");

        if let Some(ref rule_id) = id {
            info!("Removing existing rule `{rule_id}` before overwrite...");
            if let Err(e) = session.remove_rule(*rule_id).await {
                warn!("Failed to remove existing rule `{rule_id}`: {e}");
            }
        }

        info!("Saving rule...");
        let res = session.save_rule(id, text, false).await;
        info!("Rule saved. Releasing lock...");
        res
    };

    match save_res {
        Ok(rule) => {
            info!("Rule set successfully");
            Response::ok().json(&rule)
        }
        Err(e) => {
            error!("Failed to set rule for session {sid}: {e}");
            Response::error().text(e.to_string())
        }
    }
}

/// API: Removes rule from the active session context by ID.
#[log(sid = %sid)]
pub async fn handle_session_rules_remove(
    Paths(sid): Paths<SessionId>,
    query: Json<RemoveQuery>,
) -> Response {
    let rule_id = query.0.id;
    info!("Removing rule `{rule_id}`...");

    let session_shared = session!(sid);

    info!("Waiting for session lock...");
    let remove_res = {
        let session = session_shared.lock().await;
        info!("Acquired session lock. Removing rule `{rule_id}`...");
        let res = session.remove_rule(rule_id.clone()).await;
        info!("Remove operation finished. Releasing lock...");
        res
    };

    match remove_res {
        Ok(deleted) => {
            if deleted {
                info!("Rule `{rule_id}` removed successfully");
                Response::ok().text("Rule removed successfully")
            } else {
                warn!("Rule `{rule_id}` not found");
                Response::error().text(format!("Rule `{rule_id}` not found"))
            }
        }
        Err(e) => {
            error!("Failed to remove rule `{rule_id}` for session {sid}: {e}");
            Response::error().text(e.to_string())
        }
    }
}

/// API: Clears only local rules for a session.
#[log(sid = %sid)]
pub async fn handle_session_rules_clear(Paths(sid): Paths<SessionId>) -> Response {
    info!("Requesting clear_local_rules...");

    let session_shared = session!(sid);

    info!("Waiting for session lock...");
    let clear_res = {
        let session = session_shared.lock().await;
        info!("Acquired session lock. Clearing local rules...");
        let res = session.clear_rules().await;
        info!("Local rules cleared. Releasing lock...");
        res
    };

    match clear_res {
        Ok(_) => {
            info!("Local session rules cleared successfully");
            Response::ok().text("Local session rules cleared successfully")
        }
        Err(e) => {
            error!("Failed to clear local rules for session {sid}: {e}");
            Response::error().text(e.to_string())
        }
    }
}
