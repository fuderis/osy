use crate::{
    prelude::*,
    user::{Session, UserState},
};

use anylm::{Schema, Tool};
use osy_share::SessionId;

/// Returns history tools list.
pub fn tools_list() -> Vec<Tool> {
    vec![Tool::typed::<ReadHistory>(
        "read_history",
        "Reads the full original User -> Assistant message pair by turn index \
        from the session history. Use this when the dialog map is not detailed enough \
        and you need exact wording, parameters, or full context of a specific turn.",
    )]
}

#[derive(Deserialize, Schema)]
pub struct ReadHistory {
    /// Turn pair index (0-based `User -> Assistant` pair index from the dialog map).
    pub index: usize,
    /// Optional session ID. If omitted, uses the user's last active session.
    #[serde(default)]
    #[schema(serde_json)]
    pub session_id: Option<SessionId>,
}

/// Reads a specific original `(User, Assistant)` pair by index from session history.
#[log(uid = %user.id, index = data.index)]
pub async fn handle_read_history(user: &UserState, data: ReadHistory) -> Result<String> {
    let ReadHistory { index, session_id } = data;

    let sid = match session_id {
        Some(id) => id,
        None => user
            .get_last_session_id()
            .await?
            .ok_or_else(|| Error::Custom("No active session found for user".into()))?,
    };

    let Some(session_arc) = Session::get(&sid) else {
        return Err(Error::UnknownSessionId(sid).into());
    };

    let pair = {
        let session_guard = session_arc.lock().await;
        if index >= session_guard.metadata.message_count {
            return Ok(format!(
                "Turn index {index} is out of range (total pairs: {}).",
                session_guard.metadata.message_count
            ));
        }

        session_guard.read_pair(index).await?
    };

    let Some((user_msg, assistant_msg)) = pair else {
        return Ok(format!("No messages found for turn index {index}."));
    };

    let user_text = user_msg.extract_texts().join("\n");
    let assistant_text = assistant_msg.extract_texts().join("\n");

    info!("Read history pair at index {index} for session `{sid}`");

    Ok(format!(
        "History — Turn #{index} (Session `{sid}`):\n\
         User:\n{user_text}\n\n\
         Assistant:\n{assistant_text}"
    ))
}
