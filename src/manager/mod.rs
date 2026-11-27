//! Agents management module.

pub mod agent;
pub use agent::Agent;

use crate::{prelude::*, skills};

use anylm::api::Tool;
use osy_share::AgentMeta;
use std::fmt::Write;
use tokio::{fs, task::JoinSet};

/// Agents manager state.
pub static MANAGER: State<Manager> = State::default();

/// Agents manager.
#[derive(Default, Clone)]
pub struct Manager {
    /// Connected agents list {name => agent}.
    ///
    /// Agents are immutable after registration, so they are shared as plain `Arc<Agent>`
    /// (no async lock needed). Never hold a map reference across `.await`.
    pub agents: Arc<SharedMap<String, Arc<Agent>>>,
    /// Agents list documentation (skills).
    pub agents_doc: Arc<String>,
    /// Full set of tools for orchestrator (includes `use_skill`).
    pub tools: Arc<Vec<Tool>>,
    /// Basic set of tools for skills scope (excludes `use_skill`).
    pub skill_tools: Arc<Vec<Tool>>,
}

impl Manager {
    /// Initializes & runs the agents management.
    #[log()]
    pub async fn init() -> Result<()> {
        let scan_dir = path!("$/");

        // check scan dir
        if !fs::try_exists(&scan_dir).await? {
            warn!("[Manager] Core directory not found at: {scan_dir:?}.");
            return Ok(());
        }

        let mut set = JoinSet::new();
        let mut reader = fs::read_dir(&scan_dir).await?;

        info!("[Manager] Scanning for agent binaries...");

        // read files in core dir
        while let Some(entry) = reader.next_entry().await? {
            let path = entry.path();
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            // check if it's an agent binary
            if file_name.starts_with("osy-") && !Self::has_agent_path(&path).await {
                // spawn agent running:
                set.spawn(async move { Agent::run(path).await });
            }
        }

        // check results
        while let Some(task_res) = set.join_next().await {
            if let Err(e) = task_res {
                error!("[Manager] Agent startup task panicked: {e}");
            }
        }

        // gen task delegation tool
        Self::gen_basic_tools().await;

        Ok(())
    }

    /// Generates & sets the basic tools schemes.
    pub async fn gen_basic_tools() {
        let history_tools = skills::history::tools_list();
        let eval_tools = skills::eval::tools_list();
        let fact_tools = skills::fact::tools_list();
        let skill_tools = skills::skill::tools_list();

        // full list of tools for main orchestrator
        let tools: Vec<Tool> = vec![
            history_tools.clone(),
            eval_tools.clone(),
            fact_tools.clone(),
            skill_tools,
        ]
        .into_iter()
        .flatten()
        .collect();

        // skill-level basic tools (without use_skill to prevent recursion)
        let skill_basic_tools: Vec<Tool> =
            vec![eval_tools, fact_tools].into_iter().flatten().collect();

        let mut guard = MANAGER.lock().await;
        guard.tools = arc!(tools);
        guard.skill_tools = arc!(skill_basic_tools);
    }

    /// Checks & updates agents list.
    #[log()]
    pub async fn update() -> Result<()> {
        info!("[Manager] Starting agents update cycle...");

        let mut to_restart = Vec::new();
        let mut to_refresh = Vec::new();

        // sort agents into those who need to restart and those who need to refresh
        for agent in Self::snapshot() {
            if agent.check(true).await.unwrap_or(true) {
                to_restart.push(agent);
            } else {
                to_refresh.push(agent);
            }
        }

        // call /refresh only for those agents who do not go to restart
        for agent in to_refresh {
            if let Err(e) = agent.refresh().await {
                warn!(
                    "[Manager] Error refreshing agent `{}`: {e}",
                    agent.metadata.name
                );
            }
        }

        // stopping outdated agents
        for agent in to_restart {
            warn!("[Manager] Agent `{}` is outdated...", agent.metadata.name);
            agent.stop().await?;
        }

        Self::init().await?;

        info!("[Manager] Agents update cycle completed.");
        Ok(())
    }

    /// Returns a snapshot of all running agents.
    ///
    /// The map reference is released before returning, so the result is safe to hold across `.await`.
    pub fn snapshot() -> Vec<Arc<Agent>> {
        let agents = MANAGER.get().agents.clone();
        let list: Vec<_> = agents.iter().map(|e| e.value().clone()).collect();
        list
    }

    /// Generates agents documentation.
    pub(super) async fn gen_doc() {
        let agents = Self::snapshot();

        // gen message, if agents not found
        if agents.is_empty() {
            MANAGER.lock().await.agents_doc = arc!("No active skills available.".to_string());
            return;
        }

        // gen skills doc:
        let mut doc_builder = String::from("Available Skills:\n");
        for agent in &agents {
            for (_, skill) in &agent.metadata.skills {
                let _ = writeln!(
                    doc_builder,
                    "* `{}`: {}",
                    skill.name,
                    skill.description.trim().replace("\n", "")
                );
            }
        }

        MANAGER.lock().await.agents_doc = arc!(doc_builder);
        info!(
            "[Manager] Documentation updated ({} agents processed).",
            agents.len()
        );
    }

    /// Returns agents documentation for prompt.
    pub async fn agents_doc() -> Arc<String> {
        MANAGER.get().agents_doc.clone()
    }

    /// Returns basic tools list for main orchestrator.
    pub async fn basic_tools() -> Vec<Tool> {
        (*MANAGER.get().tools).clone()
    }

    /// Returns basic tools list for skill execution context.
    pub async fn skill_basic_tools() -> Vec<Tool> {
        (*MANAGER.get().skill_tools).clone()
    }

    /// Returns agents list.
    pub async fn agents_list() -> Vec<AgentMeta> {
        Self::snapshot()
            .into_iter()
            .map(|agent| agent.metadata.clone())
            .collect()
    }

    /// Returns true if agent with this name on running.
    pub async fn has_agent(name: &Arc<String>) -> bool {
        MANAGER.get().agents.contains_key(name.as_str())
    }

    /// Returns true if agent with this path on running.
    pub async fn has_agent_path(path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        MANAGER
            .get()
            .agents
            .iter()
            .any(|e| e.value().exec_path == path)
    }

    /// Returns agent reference by name.
    pub async fn get_agent(name: &String) -> Option<Arc<Agent>> {
        let agents = MANAGER.get().agents.clone();
        let found = agents.get(name).map(|e| e.value().clone());
        found
    }

    /// Returns agent reference by skill name.
    pub async fn get_by_skill(skill: &str) -> Option<Arc<Agent>> {
        let agents = MANAGER.get().agents.clone();
        let found = agents
            .iter()
            .find(|e| e.value().metadata.skills.contains_key(skill))
            .map(|e| e.value().clone());
        found
    }
}
