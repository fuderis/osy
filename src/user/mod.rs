//! Users management module.

pub mod session;
pub use session::Session;

use std::sync::Arc;

use crate::{prelude::*, utils};

use anylm::embeddings::Search;
use atoman::dashmap::DashMap;
use cistern::{
    Context, Storage,
    context::{ScoredRecord, SearchOptions},
    gen_id,
};
use osy_share::{SessionId, SessionMetadata, UserFact, UserMetadata, UserRule};
use tokio::sync::OnceCell;

/// Result of saving a fact.
pub enum SaveFact {
    /// The fact was written (new ID).
    Saved(u64),
    /// The same fact already exists, nothing was changed.
    Duplicate,
    /// Near-duplicates exist, nothing was changed (the caller decides whether to replace one of them).
    Similar(Vec<UserFact>),
}

/// Lazily initialized user slot (a slot is created atomically, the state is built once).
type UserSlot = Arc<OnceCell<Arc<UserState>>>;

/// Active users map {uid => slot}.
static USERS: SharedMap<u64, UserSlot> = SharedMap::new();

const FACTS_TABLE_NAME: &str = "facts";
const RULES_TABLE_NAME: &str = "rules";

/// User state.
///
/// Shared as `Arc<UserState>` without any outer lock: all fields are either immutable
/// or have their own interior synchronization.
pub struct UserState {
    /// Unique user identifier.
    pub id: u64,
    /// Key-Value database for user metadata & rules.
    pub kv_db: Arc<Storage>,
    /// Vector database for user facts.
    pub rag_db: Arc<Context>,
    /// Active user sessions.
    sessions: DashMap<SessionId, session::SharedSession>,
    /// Serializes read-modify-write cycles of the user metadata.
    meta_lock: Mutex<()>,
    /// Serializes initialization of user sessions.
    init_lock: Mutex<()>,
}

impl UserState {
    /// Returns active user state (without initializing it).
    pub fn get(id: u64) -> Option<Arc<UserState>> {
        USERS.get(&id).and_then(|slot| slot.get().cloned())
    }

    /// Returns user state or initializes it (concurrent callers share a single initialization).
    pub async fn get_or_init(id: u64) -> Result<Arc<UserState>> {
        // fast path: active user
        if let Some(user) = Self::get(id) {
            return Ok(user);
        }

        // take (or create) the slot, the map reference is dropped at the end of the statement
        let slot: UserSlot = USERS.entry(id).or_default().clone();

        let user = slot
            .get_or_try_init(|| async { Self::connect(id).await.map(Arc::new) })
            .await?;

        Ok(user.clone())
    }

    /// Opens user databases.
    async fn connect(id: u64) -> Result<Self> {
        let user_base = path!("$share$/users/{id}");

        let kv_db = Arc::new(Storage::connect(user_base.join("userdata")).await?);
        let rag_db = Arc::new(Context::connect(user_base.join("context")).await?);

        Ok(Self {
            id,
            kv_db,
            rag_db,
            sessions: DashMap::new(),
            meta_lock: Mutex::new(()),
            init_lock: Mutex::new(()),
        })
    }

    /// Returns true if this state is still the registered one (was not evicted).
    fn is_live(&self) -> bool {
        USERS
            .get(&self.id)
            .and_then(|slot| slot.get().map(|u| std::ptr::eq(Arc::as_ptr(u), self)))
            .unwrap_or(false)
    }

    /// Frees user state from memory if it has no active sessions.
    fn evict_if_idle(id: u64) {
        USERS.remove_if(&id, |_, slot| {
            slot.get().is_some_and(|user| user.sessions.is_empty())
        });
    }

    /// Returns active session of this user.
    pub fn session(&self, sid: &SessionId) -> Option<session::SharedSession> {
        self.sessions.get(sid).map(|s| s.value().clone())
    }

    /// Updates user metadata atomically (read-modify-write under the metadata lock).
    pub async fn update_metadata<R>(&self, f: impl FnOnce(&mut UserMetadata) -> R) -> Result<R> {
        let _guard = self.meta_lock.lock().await;

        let mut meta = self.load_metadata().await?.unwrap_or_default();
        let res = f(&mut meta);
        self.save_metadata(&meta).await?;

        Ok(res)
    }

    /// Returns list of user sessions metadata (ordered by freshness).
    pub async fn list_sessions(id: u64, limit: usize) -> Result<Vec<SessionMetadata>> {
        let session_ids = Self::get_or_init(id)
            .await?
            .load_metadata()
            .await?
            .unwrap_or_default()
            .sessions;

        let mut session_ids = session_ids;
        session_ids.reverse();
        if limit > 0 && session_ids.len() > limit {
            session_ids.truncate(limit);
        }

        let mut result = Vec::with_capacity(session_ids.len());
        for sid in session_ids {
            // check active session in memory to avoid redundant DB reads
            if let Some(session) = session::Session::get(&sid) {
                let meta = session.lock().await.metadata.clone();
                result.push(meta);
            } else {
                // retrieve from disk for offline sessions
                let user_base = path!("$share$/users/{id}");
                let session_dir = user_base.join("sessions").join(sid.to_string());

                if let Ok(kv_db) = Storage::connect(session_dir).await {
                    let table_name = str!(sid);
                    if let Ok(table) = kv_db.open_table(&table_name).await {
                        if let Ok(Some(meta)) = table
                            .read::<_, SessionMetadata>(session::Key::Metadata)
                            .await
                        {
                            result.push(meta);
                            continue;
                        }
                    }
                }

                // fallback if metadata file is missing or corrupted
                result.push(SessionMetadata {
                    session_id: sid,
                    ..Default::default()
                });
            }
        }

        Ok(result)
    }

    /// Loads user's metadata from database.
    pub async fn load_metadata(&self) -> Result<Option<UserMetadata>> {
        let table = self.kv_db.open_table("global").await?;
        table.read("user_metadata").await.map_err(Into::into)
    }

    /// Retains user's metadata into database.
    pub async fn save_metadata(&self, meta: &UserMetadata) -> Result<()> {
        let table = self.kv_db.open_table("global").await?;

        table.write("user_metadata", meta.clone()).await?;
        table.flush().await?;

        Ok(())
    }

    /// Returns last used session ID.
    pub async fn get_last_session_id(&self) -> Result<Option<SessionId>> {
        let meta = self.load_metadata().await?.unwrap_or_default();
        Ok(meta.last_session)
    }

    /// Removes user session (from memory, metadata and session database).
    pub async fn remove_session(&self, session_id: &SessionId) -> Result<()> {
        // remove from memory
        self.sessions.remove(session_id);

        // remove from metadata
        self.update_metadata(|meta| {
            meta.sessions.retain(|s| s != session_id);
            if meta.last_session == Some(*session_id) {
                meta.last_session = meta.sessions.last().copied();
            }
        })
        .await?;

        // clear session database
        let session_table_name = str!(session_id);
        let table = self.kv_db.open_table(&session_table_name).await?;

        table.clear().await?;
        table.flush().await?;

        Ok(())
    }
}

impl UserState {
    /// Returns list of all user's rules (global).
    pub async fn load_rules(&self) -> Result<Vec<UserRule>> {
        let table = self.kv_db.open_table(RULES_TABLE_NAME).await?;

        Ok(table
            .read_all::<u64, UserRule>()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.1)
            .collect())
    }

    /// Retains new user rule (global).
    pub async fn save_rule(&self, id: Option<u64>, text: String) -> Result<UserRule> {
        let rule_id = id.unwrap_or_else(gen_id);
        let rule = UserRule {
            id: rule_id,
            text,
            is_global: true,
            created_at: Utc::now(),
        };

        let table = self.kv_db.open_table(RULES_TABLE_NAME).await?;

        table.write(rule_id, rule.clone()).await?;
        table.flush().await?;

        Ok(rule)
    }

    /// Removes user rule by ID (global).
    pub async fn remove_rule(&self, id: u64) -> Result<bool> {
        let table = self.kv_db.open_table(RULES_TABLE_NAME).await?;

        if table.read::<_, UserRule>(id).await?.is_some() {
            table.remove(id).await?;
            table.flush().await?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Removes all user's rules.
    pub async fn clear_rules(&self) -> Result<()> {
        let table = self.kv_db.open_table(RULES_TABLE_NAME).await?;

        table.clear().await?;
        table.flush().await?;

        Ok(())
    }
}

impl UserState {
    /// Retains new user fact.
    ///
    /// Facts are embedded as written (no translation and no LLM rewriting). Nothing is ever deleted silently:
    /// * an exact duplicate (case/whitespace-insensitive) is a no-op;
    /// * `replaces` removes exactly that fact and writes the new one;
    /// * with `guard_similar`, near-duplicates are returned to the caller instead of being overwritten.
    pub async fn save_fact(
        &self,
        text: impl Into<String>,
        replaces: Option<u64>,
        guard_similar: bool,
    ) -> Result<SaveFact> {
        let text = text.into().trim().to_owned();
        if text.is_empty() {
            return Err(Error::Custom("Fact text cannot be empty.".into()).into());
        }

        let embedding = utils::generate_embedding(&text, Search::Document).await?;

        // look for near-duplicates
        let table = self.rag_db.open_table(FACTS_TABLE_NAME).await?;
        let dedup_threshold = Config::get().context.dedup_threshold();

        let neighbours: Vec<UserFact> = match table
            .read::<UserFact>(embedding.clone(), Some(5), dedup_threshold)
            .await
        {
            Ok(Some(records)) => records.into_iter().map(|r| r.data).collect(),
            _ => vec![],
        };

        let key = utils::fact_key(&text);
        if neighbours.iter().any(|f| utils::fact_key(&f.text) == key) {
            return Ok(SaveFact::Duplicate);
        }

        if let Some(old_id) = replaces {
            // explicit replacement requested by the caller
            let _ = table.remove(old_id).await;
        } else if guard_similar && !neighbours.is_empty() {
            return Ok(SaveFact::Similar(neighbours.into_iter().take(3).collect()));
        }

        // write fact to database
        let fact_id = gen_id();
        let fact = UserFact {
            id: fact_id,
            // kept for storage compatibility: the embedding is built from the original text
            search_text: text.clone(),
            text,
            created_at: Utc::now(),
        };

        table.write(fact_id, embedding, fact).await?;
        Ok(SaveFact::Saved(fact_id))
    }

    /// Searches user's facts in database (hybrid: by meaning and by exact words).
    ///
    /// The query is embedded as is: the embeddings model is multilingual, so no translation is needed.
    /// Only relevant facts are returned: cosine similarity above the floor, close to the best hit,
    /// plus exact word matches (names, versions, models).
    pub async fn search_facts(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<ScoredRecord<UserFact>>> {
        let ctx = &Config::get().context;
        let max = ctx.limit();
        let limit = limit.unwrap_or(max).clamp(1, max * 2);

        self.run_fact_search(
            query,
            limit,
            ctx.similarity_threshold(),
            ctx.margin(),
            ctx.hybrid_search,
        )
        .await
    }

    /// Searches facts WITHOUT thresholds and with scores: for calibrating `fact_similarity` and `fact_margin`.
    pub async fn probe_facts(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ScoredRecord<UserFact>>> {
        let hybrid = Config::get().context.hybrid_search;
        self.run_fact_search(query, limit.clamp(1, 50), -1.0, None, hybrid)
            .await
    }

    async fn run_fact_search(
        &self,
        query: &str,
        limit: usize,
        min_similarity: f32,
        margin: Option<f32>,
        hybrid: bool,
    ) -> Result<Vec<ScoredRecord<UserFact>>> {
        let query = utils::fact_query(query);
        if query.is_empty() {
            return Err(Error::Custom("Search query cannot be empty.".into()).into());
        }

        // generate query embedding
        let embedding = utils::generate_embedding(&query, Search::Query).await?;

        // search facts in database
        let table = self.rag_db.open_table(FACTS_TABLE_NAME).await?;
        let found = table
            .search::<UserFact>(
                embedding,
                SearchOptions {
                    limit,
                    min_similarity,
                    margin,
                    text: hybrid.then(|| query.clone()),
                    lexical_fields: Some(vec!["text".to_owned()]),
                },
            )
            .await?;

        Ok(found)
    }

    /// Re-embeds ALL facts from their original text and atomically replaces the table content.
    ///
    /// Needed after changing the embeddings model or the indexing algorithm (old facts were embedded
    /// from an LLM-expanded English text). Nothing is written until all embeddings are ready.
    pub async fn reindex_facts(&self) -> Result<usize> {
        let facts = self.load_facts().await?;
        if facts.is_empty() {
            return Ok(0);
        }

        let mut items = Vec::with_capacity(facts.len());
        for mut fact in facts {
            let embedding = utils::generate_embedding(&fact.text, Search::Document).await?;
            fact.search_text = fact.text.clone();
            items.push((fact.id, embedding, fact));
        }

        let count = items.len();
        let table = self.rag_db.open_table(FACTS_TABLE_NAME).await?;
        table.replace_all(items).await?;
        table.ensure_index().await?;

        Ok(count)
    }

    /// Returns list of all user's facts.
    pub async fn load_facts(&self) -> Result<Vec<UserFact>> {
        let table = self.rag_db.open_table(FACTS_TABLE_NAME).await?;
        let records = table.read_all().await?;

        Ok(records
            .unwrap_or_default()
            .into_iter()
            .map(|f| f.data)
            .collect())
    }

    /// Removes user fact by ID.
    pub async fn remove_fact(&self, fact_id: u64) -> Result<()> {
        let table = self.rag_db.open_table(FACTS_TABLE_NAME).await?;
        table.remove(fact_id).await?;
        Ok(())
    }

    /// Removes all user's facts.
    pub async fn clear_facts(&self) -> Result<()> {
        let table = self.rag_db.open_table(FACTS_TABLE_NAME).await?;
        table.clear().await?;
        Ok(())
    }
}
