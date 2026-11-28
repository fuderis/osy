use crate::{handlers, prelude::*};

use anylm::{Content, Message, Messages, Schema, Tool};
use osy_share::{Event, SessionInfo};

/// Returns tools list.
pub fn tools_list() -> Vec<Tool> {
    vec![Tool::typed::<SkillAction>(
        "use_skill",
        "Delegates a task using a specific skill.",
    )]
}

/// Agent skill task data.
#[derive(Deserialize, Schema)]
pub struct SkillAction {
    #[schema(skip)]
    #[serde(default)]
    pub tool_call_id: String,

    /// The existing skill required for this task.
    pub skill: String,
    /// The detailed task prompt, parameters, and input context.
    pub query: String,
}

/// Handles agent skill (isolated).
///
/// A failed skill does not abort the user's query: the error is written into the tool result,
/// so the orchestrator can retry with other parameters or honestly report the failure.
/// Successfully finished sibling tasks are never re-run because of it.
#[log(skill = %task.skill)]
pub async fn handle_skill(
    tx: Sender<Bytes>,
    session_info: SessionInfo,
    messages: Arc<Mutex<Messages>>,
    current_path: Option<PathBuf>,
    task: SkillAction,
) -> Result<()> {
    let skill_name = task.skill.clone();

    let skill_response = match handlers::handle_skill(
        tx.clone(),
        session_info,
        &task.skill,
        Message::user(vec![task.query.into()]),
        current_path,
        false,
    )
    .await
    {
        Ok(text) => text,
        // the client has gone: nothing to heal
        Err(e) if tx.is_closed() => return Err(e),
        Err(e) => {
            warn!("Skill `{skill_name}` failed: {e}");
            tx.send(Event::Thinking(format!("Skill `{skill_name}` failed: {e}")).into())
                .await
                .ok();
            format!(
                "Skill `{skill_name}` failed: {e}\n\
                 You may retry once with corrected parameters. If it fails again, tell the user what went wrong."
            )
        }
    };

    // recording result of skill execution in the parent message context
    messages
        .lock()
        .await
        .push_content(Some(&task.tool_call_id), Content::text(skill_response));

    Ok(())
}
