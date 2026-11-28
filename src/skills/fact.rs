use crate::{
    prelude::*,
    user::{SaveFact, UserState},
};

use anylm::{Schema, Tool};

/// Returns tools list.
pub fn tools_list() -> Vec<Tool> {
    vec![
        Tool::typed::<RememberFact>(
            "remember_fact",
            "Saves a new persistent fact or memory about the user into long-term memory. \
            Use this when the user explicitly asks to remember something or discloses important user-specific information \
            (e.g., preferences, personal facts, project settings, tech stack).",
        ),
        Tool::typed::<SearchFact>(
            "search_fact",
            "Searches long-term user memory by meaning. Relevant facts are usually preloaded in context already: \
            use this only when a specific detail is missing. Returns matching facts with their IDs.",
        ),
    ]
}

#[derive(Deserialize, Schema)]
pub struct RememberFact {
    /// One self-contained statement about the user, in the user's language (name the subject explicitly,
    /// keep names, numbers and versions).
    pub text: String,
    /// ID of an existing fact that this one updates or replaces (taken from a "similar fact exists" reply).
    #[serde(default)]
    pub replaces: Option<u64>,
    /// Set to true to store this as a separate fact even if a similar one exists.
    #[serde(default)]
    pub keep_both: bool,
}

#[derive(Deserialize, Schema)]
pub struct SearchFact {
    /// A short search query (2-6 words) in the user's language.
    pub query: String,
    /// A limit on number of facts that must be found.
    #[serde(default)]
    pub limit: Option<usize>,
}

// --- Action Payload Deserializers ---

/// Outcome of the `remember_fact` tool.
pub struct MemoryOutcome {
    /// Text for the model.
    pub text: String,
    /// True if the model has nothing more to do with this result (it may answer right away).
    pub silent: bool,
}

/// Saves new fact to the user's vector storage.
#[log(uid = %user.id)]
pub async fn handle_remember_fact(user: &UserState, data: RememberFact) -> Result<MemoryOutcome> {
    let RememberFact {
        text,
        replaces,
        keep_both,
    } = data;

    // save fact to database (near-duplicates are returned to the model, not overwritten)
    let outcome = match user.save_fact(text.clone(), replaces, !keep_both).await {
        Ok(res) => res,
        Err(e) => {
            error!("Failed to save user fact: {e}");
            return Err(e);
        }
    };

    Ok(match outcome {
        SaveFact::Saved(_) => {
            info!("Saved new user fact: `{text}`");
            MemoryOutcome {
                text: format!("Fact saved: \"{text}\""),
                silent: true,
            }
        }
        SaveFact::Duplicate => MemoryOutcome {
            text: "This fact is already stored. Nothing changed.".into(),
            silent: true,
        },
        SaveFact::Similar(similar) => {
            let mut msg = String::from("Not saved: similar fact(s) already exist:\n");
            for fact in similar {
                msg.push_str(&format!("* [ID: {}] {}\n", fact.id, fact.text));
            }
            msg.push_str(
                "If the new fact updates one of them, call remember_fact again with `replaces` set to that ID. \
                 If it is a different fact, call it again with `keep_both` = true. Otherwise ignore this.",
            );
            MemoryOutcome {
                text: msg,
                silent: false,
            }
        }
    })
}

/// Searches for relevant user facts.
#[log(uid = %user.id)]
pub async fn handle_search_fact(user: &UserState, data: SearchFact) -> Result<String> {
    let SearchFact { query, limit } = data;

    // search facts in database
    let records = match user.search_facts(&query, limit).await {
        Ok(res) => res,
        Err(e) => {
            error!("Failed to search user's facts: {e}");
            return Err(e);
        }
    };

    // check records len
    if records.is_empty() {
        info!("Search for fact `{query}` returned no results.");
        return Ok(format!("No relevant facts found for query: \"{query}\""));
    }
    info!("Found {} facts for query `{query}`", records.len());

    // collect response
    let mut response = format!(
        "Found {} relevant facts for query \"{query}\":\n",
        records.len()
    );
    for record in records {
        response.push_str(&format!("* [ID: {}] {}\n", record.id, record.data.text));
    }

    Ok(response)
}
