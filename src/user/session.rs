use super::*;
use crate::prelude::*;

use anylm::api::{Message, Messages};
use cistern::{Storage, gen_id};
use osy_share::{SessionId, SessionInfo, SessionMetadata, UserRule};

pub type SharedSession = Arc<Mutex<Session>>;
pub type MessagePair = (Message, Message);

/// Session table key.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Key {
    /// Session metadata.
    Metadata,
    /// Original message pair (`User -> Assistant`) at turn index.
    Message(usize),
}

/// User session manager.
pub struct Session {
    /// Unique session identifier.
    pub id: SessionId,
    /// Session info and properties.
    pub info: SessionInfo,
    /// Key-Value storage for session metadata and history.
    pub kv_db: Arc<Storage>,
    /// User identifier.
    pub user_id: u64,
    /// Session metadata cached in-memory.
    pub metadata: SessionMetadata,
}

impl Session {
    /// Returns active session from memory (two map lookups, no I/O).
    pub fn get(sid: &SessionId) -> Option<SharedSession> {
        UserState::get(sid.user_id as u64)?.session(sid)
    }

    /// Returns active session or `UnknownSessionId` error.
    pub fn require(sid: &SessionId) -> Result<SharedSession> {
        Self::get(sid).ok_or_else(|| Error::UnknownSessionId(*sid).into())
    }

    /// Returns active session or initializes it.
    pub async fn get_or_init(sid: SessionId, info: SessionInfo) -> Result<SharedSession> {
        match Self::get(&sid) {
            Some(session) => Ok(session),
            None => Self::init(sid, info).await,
        }
    }

    /// Reads user session from database and returns original history pairs flattened.
    #[log(sid = %sid)]
    pub async fn read(sid: SessionId) -> Result<(SharedSession, Arc<Mutex<Messages>>)> {
        info!("[Session] Reading user session `{sid}`...");

        let session = Self::require(&sid)?;
        let db_messages = session.lock().await.read_messages().await?;
        let messages = Arc::new(Mutex::new(Messages::from(db_messages)));

        Ok((session, messages))
    }

    /// Initializes user session (or returns the already active one).
    #[log(sid = %sid)]
    pub async fn init(sid: SessionId, info: SessionInfo) -> Result<SharedSession> {
        info!("[Session] Initializing session `{sid}`...");

        let uid = sid.user_id as u64;

        loop {
            let user = UserState::get_or_init(uid).await?;

            // concurrent initializations of the same user are serialized
            let _init_guard = user.init_lock.lock().await;
            if let Some(existing) = user.session(&sid) {
                return Ok(existing);
            }

            let user_base = path!("$share$/users/{uid}");
            let session_dir = user_base.join("sessions").join(sid.to_string());
            let kv_db = Arc::new(Storage::connect(session_dir).await?);

            let table_name = str!(sid);
            let table = kv_db.open_table(&table_name).await?;

            let metadata = match table.read(Key::Metadata).await? {
                Some(meta) => meta,
                None => {
                    let new_meta = SessionMetadata {
                        session_id: sid,
                        ..Default::default()
                    };
                    table.write(Key::Metadata, new_meta.clone()).await?;
                    table.flush().await?;
                    new_meta
                }
            };

            let this = Arc::new(Mutex::new(Self {
                id: sid,
                info: info.clone(),
                kv_db,
                user_id: uid,
                metadata,
            }));

            user.sessions.insert(sid, this.clone());

            // the user state could have been evicted while we were initializing: retry on a fresh one
            if !user.is_live() {
                user.sessions.remove(&sid);
                continue;
            }

            user.update_metadata(|meta| {
                if !meta.sessions.contains(&sid) {
                    meta.sessions.push(sid);
                }
                meta.last_session = Some(sid);
            })
            .await?;

            info!("[Session] Session initialized successfully.");
            return Ok(this);
        }
    }

    /// Flushes unsaved session changes to database.
    async fn flush(&self) -> Result<()> {
        let table_name = str!(self.id);
        let table = self.kv_db.open_table(&table_name).await?;
        table.flush().await?;
        Ok(())
    }

    /// Finishes user session, flushes and cleans up memory.
    /// If session belongs to temporary user (uid == 0), completely purges it from disk.
    #[log(sid = %sid)]
    pub async fn finish(sid: &SessionId) -> Result<()> {
        info!("[Session] Initiating session completion `{sid}`...");

        let uid = sid.user_id as u64;

        // if this is a private user (uid == 0), erase it completely
        if uid == 0 {
            info!("[Session] Ephemeral user detected (uid=0). Purging session files.");
            return Self::remove(sid).await;
        }

        let Some(user) = UserState::get(uid) else {
            return Ok(());
        };

        // remove session from memory and flush unsaved changes to database
        let flushed = match user.sessions.remove(sid) {
            Some((_, session)) => session.lock().await.flush().await,
            None => Ok(()),
        };

        // if no more active sessions - clean up user's state from memory
        drop(user);
        UserState::evict_if_idle(uid);

        flushed?;
        info!("[Session] Session finished successfully.");

        Ok(())
    }

    /// Completely removes user session from memory AND disk.
    #[log(sid = %sid)]
    pub async fn remove(sid: &SessionId) -> Result<()> {
        info!("[Session] Initiating full session removal `{sid}`...");

        let uid = sid.user_id as u64;

        // delete session from user state and clear its ID from metadata
        let removed_session = match UserState::get_or_init(uid).await {
            Ok(user) => {
                let removed = user.sessions.remove(sid).map(|(_, s)| s);

                let res = user
                    .update_metadata(|meta| {
                        meta.sessions.retain(|s| s != sid);
                        if meta.last_session == Some(*sid) {
                            meta.last_session = meta.sessions.last().copied();
                        }
                    })
                    .await;
                if let Err(e) = res {
                    warn!("[Session] Failed to update user metadata: {e}");
                }

                removed
            }
            Err(_) => None,
        };

        // flush session tables and release the lock before deleting files
        if let Some(session) = removed_session {
            session.lock().await.flush().await.ok();
        }

        // delete ONLY folder of this particular session
        let session_dir = path!("$share$/users/{uid}/sessions/{sid}");
        if tokio::fs::metadata(&session_dir).await.is_ok() {
            if let Err(e) = tokio::fs::remove_dir_all(&session_dir).await {
                error!("[Session] Failed to remove session directory `{session_dir:?}`: {e}");
            }
        }

        // free user state from RAM if no active sessions in memory
        UserState::evict_if_idle(uid);

        info!("[Session] Session `{sid}` removed completely.");

        Ok(())
    }
}

impl Session {
    /// Reads session metadata.
    #[log(sid = %self.id)]
    pub async fn read_metadata(&self) -> Result<Option<SessionMetadata>> {
        info!("[Session] Reading session metadata...");
        Ok(Some(self.metadata.clone()))
    }

    /// Reads original `(User, Assistant)` message pair by index.
    #[log(sid = %self.id, index = index)]
    pub async fn read_pair(&self, index: usize) -> Result<Option<MessagePair>> {
        if index >= self.metadata.message_count {
            return Ok(None);
        }

        let table_name = str!(self.id);
        let table = self.kv_db.open_table(&table_name).await?;

        table
            .read::<_, MessagePair>(Key::Message(index))
            .await
            .map_err(Into::into)
    }

    /// Reads the last `(User, Assistant)` message pair if at least one exists.
    pub async fn read_last_pair(&self) -> Result<Option<MessagePair>> {
        match self.metadata.message_count.checked_sub(1) {
            Some(last_idx) => self.read_pair(last_idx).await,
            None => Ok(None),
        }
    }

    /// Reads all original session messages flattened in chronological order (`0..message_count`).
    #[log(sid = %self.id)]
    pub async fn read_messages(&self) -> Result<Vec<Message>> {
        info!("[Session] Reading session messages...");

        let table_name = str!(self.id);
        let table = self.kv_db.open_table(&table_name).await?;

        let count = self.metadata.message_count;
        let mut messages = Vec::with_capacity(count * 2);

        for i in 0..count {
            if let Some((user_msg, assistant_msg)) =
                table.read::<_, MessagePair>(Key::Message(i)).await?
            {
                messages.push(user_msg);
                messages.push(assistant_msg);
            }
        }

        Ok(messages)
    }

    /// Builds a compact dialog map with each turn's text truncated to `max_chars` per side.
    #[log(sid = %self.id, max_chars = max_chars)]
    pub async fn dialog_map(&self, max_chars: usize) -> Result<String> {
        let count = self.metadata.message_count;
        if count == 0 {
            return Ok(String::new());
        }

        let table_name = str!(self.id);
        let table = self.kv_db.open_table(&table_name).await?;

        let mut out = String::from("Dialog map ([ID] User -> Assistant):\n");
        for i in 0..count {
            if let Some((user_msg, assistant_msg)) =
                table.read::<_, MessagePair>(Key::Message(i)).await?
            {
                let u = utils::truncate_inline(&user_msg.extract_texts().join(" "), max_chars);
                let a = utils::truncate_inline(&assistant_msg.extract_texts().join(" "), max_chars);
                out.push_str(&format!("[{i}] {u} -> {a}\n"));
            }
        }
        out.push_str(
            "Use `read_history` with turn ID to inspect full original messages if needed.",
        );

        Ok(out)
    }

    /// Writes a new original `(User, Assistant)` turn pair and returns its index.
    #[log(sid = %self.id)]
    pub async fn write_messages(&mut self, pair: MessagePair) -> Result<usize> {
        info!("[Session] Writing new session turn pair...");

        let index = self.metadata.message_count;
        let table_name = str!(self.id);
        let table = self.kv_db.open_table(&table_name).await?;

        table.write(Key::Message(index), pair).await?;

        self.metadata.message_count += 1;
        table.write(Key::Metadata, self.metadata.clone()).await?;
        table.flush().await?;

        Ok(index)
    }

    /// Clears session messages and resets metadata counters.
    #[log(sid = %self.id)]
    pub async fn clear(&mut self) -> Result<()> {
        info!("[Session] Clearing message history...");

        let table_name = str!(self.id);
        let table = self.kv_db.open_table(&table_name).await?;

        for i in 0..self.metadata.message_count {
            table.remove(Key::Message(i)).await?;
        }

        self.metadata = SessionMetadata {
            session_id: self.id,
            ..Default::default()
        };

        table.write(Key::Metadata, self.metadata.clone()).await?;
        table.flush().await?;

        info!("[Session] Message history has been cleared.");

        Ok(())
    }
}

impl Session {
    /// Returns state of the session's owner.
    async fn user(&self) -> Result<Arc<UserState>> {
        UserState::get_or_init(self.user_id).await
    }

    /// Saves user rule to session (local or global).
    pub async fn save_rule(
        &self,
        id: Option<u64>,
        text: String,
        is_global: bool,
    ) -> Result<UserRule> {
        if is_global {
            self.user().await?.save_rule(id, text).await
        } else {
            let rule_id = id.unwrap_or_else(gen_id);
            let rule = UserRule {
                id: rule_id,
                text,
                is_global: false,
                created_at: Utc::now(),
            };

            let table = self.kv_db.open_table(RULES_TABLE_NAME).await?;
            table.write(rule_id, rule.clone()).await?;
            table.flush().await?;

            Ok(rule)
        }
    }

    /// Reads user rule by ID.
    pub async fn read_rule(&self, id: u64) -> Result<Option<UserRule>> {
        let local_table = self.kv_db.open_table(RULES_TABLE_NAME).await?;
        if let Some(rule) = local_table.read::<_, UserRule>(id).await? {
            return Ok(Some(rule));
        }

        let user = self.user().await?;
        let global_table = user.kv_db.open_table(RULES_TABLE_NAME).await?;
        global_table
            .read::<_, UserRule>(id)
            .await
            .map_err(Into::into)
    }

    /// Removes user rule by ID.
    pub async fn remove_rule(&self, id: u64) -> Result<bool> {
        let local_table = self.kv_db.open_table(RULES_TABLE_NAME).await?;
        if local_table.read::<_, UserRule>(id).await?.is_some() {
            local_table.remove(id).await?;
            local_table.flush().await?;
            return Ok(true);
        }

        self.user().await?.remove_rule(id).await
    }

    /// Loads all user's rules (global + local).
    pub async fn load_rules(&self) -> Result<Vec<UserRule>> {
        let mut rules = self.user().await?.load_rules().await?;

        let local_table = self.kv_db.open_table(RULES_TABLE_NAME).await?;
        if let Ok(local_rules) = local_table.read_all::<u64, UserRule>().await {
            rules.extend(local_rules.into_iter().map(|r| r.1));
        }

        Ok(rules)
    }

    /// Removes all local session's rules.
    pub async fn clear_rules(&self) -> Result<()> {
        let table = self.kv_db.open_table(RULES_TABLE_NAME).await?;
        table.clear().await?;
        table.flush().await?;

        Ok(())
    }

    /// Removes all user's rules.
    pub async fn clear_all_rules(&self) -> Result<()> {
        self.user().await?.clear_rules().await
    }
}

impl Session {
    /// Renames the session by updating its title in metadata.
    #[log(sid = %self.id)]
    pub async fn rename(&mut self, title: impl Into<String>) -> Result<()> {
        let new_title = title.into();
        info!("[Session] Renaming session to `{new_title}`...");

        let table_name = str!(self.id);
        let table = self.kv_db.open_table(&table_name).await?;

        self.metadata.title = Some(new_title);

        table.write(Key::Metadata, self.metadata.clone()).await?;
        table.flush().await?;

        info!("[Session] Session renamed successfully.");

        Ok(())
    }

    /// Duplicates session with all message pairs & local rules.
    #[log(sid = %self.id)]
    pub async fn duplicate(&self) -> Result<SessionId> {
        info!("[Session] Cloning session...");

        let new_id = SessionId::new(self.id.user_id);

        let user_base = path!("$share$/users/{}", new_id.user_id);
        let new_session_dir = user_base.join("sessions").join(new_id.to_string());
        let new_kv_db = Arc::new(Storage::connect(new_session_dir).await?);

        let src_table_name = str!(self.id);
        let dst_table_name = str!(new_id);

        let src_table = self.kv_db.open_table(&src_table_name).await?;
        let dst_table = new_kv_db.open_table(&dst_table_name).await?;

        let mut new_meta = self.metadata.clone();
        new_meta.session_id = new_id;

        for i in 0..new_meta.message_count {
            let msg_key = Key::Message(i);
            if let Some(pair) = src_table.read::<_, MessagePair>(msg_key.clone()).await? {
                dst_table.write(msg_key, pair).await?;
            }
        }

        dst_table.write(Key::Metadata, new_meta.clone()).await?;
        dst_table.flush().await?;

        let src_rules_table = self.kv_db.open_table(RULES_TABLE_NAME).await?;
        let dst_rules_table = new_kv_db.open_table(RULES_TABLE_NAME).await?;

        if let Ok(local_rules) = src_rules_table.read_all::<u64, UserRule>().await {
            for (rule_id, rule) in local_rules {
                dst_rules_table.write(rule_id, rule).await?;
            }
            dst_rules_table.flush().await?;
        }

        let new_session = Arc::new(Mutex::new(Self {
            id: new_id,
            info: self.info.clone(),
            kv_db: new_kv_db,
            user_id: self.user_id,
            metadata: new_meta,
        }));

        let user = self.user().await?;
        user.sessions.insert(new_id, new_session);
        user.update_metadata(|meta| {
            if !meta.sessions.contains(&new_id) {
                meta.sessions.push(new_id);
            }
            meta.last_session = Some(new_id);
        })
        .await?;

        info!("[Session] Session cloned to `{new_id}`.");

        Ok(new_id)
    }
}
