use crate::prelude::*;

use atoman::{Filters, Source, Tracer};
use rigging::widgets::Print;
use std::future::pending;

/// Server status.
#[derive(Deserialize)]
struct StatusResponse {
    /// List of active agents.
    pub agents_list: Vec<AgentMeta>,
}

/// Agent metadata (minimal).
#[derive(Deserialize)]
struct AgentMeta {
    /// Agent name.
    pub name: String,
}

/// Streams server logs.
pub async fn handle_trace(uid_filter: Option<u64>, only_new: bool) -> Result<()> {
    let settings = Config::get();
    let port = settings.server.port;
    let base_url = format!("http://127.0.0.1:{port}");

    // query to `/status` endpoint
    let agents: Vec<AgentMeta> = match Client::tcp()
        .get(&format!("{base_url}/status"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => res
            .json::<StatusResponse>()
            .await
            .map(|r| r.agents_list)
            .unwrap_or_default(),
        _ => {
            Print::warn(format!("Server is unreachable on port {port}."))
                .render()
                .await?;

            vec![]
        }
    };

    // build list of log sources
    let mut sources = Vec::new();

    // primary kernel source
    let kernel_name = path!("$")
        .file_name()
        .map(|s| str!(s.to_string_lossy()))
        .unwrap_or("osy".into());
    sources.push(Source::new(
        kernel_name.clone(),
        path!("$state/{kernel_name}/logs"),
        true,
        only_new,
    ));

    // dynamic agent sources
    for agent in agents {
        let agent_name = format!("{kernel_name}-{}", agent.name);
        let agent_dir = path!("$state/{agent_name}/logs");
        sources.push(Source::new(agent_name, agent_dir, true, only_new));
    }

    // configure log entry filtering by UID parameter
    let uid_str;
    let kv_pairs: Vec<(&str, &str)> = if let Some(uid) = uid_filter {
        uid_str = uid.to_string();
        Print::info(format!("Filtering logs by User ID: {uid}"))
            .render()
            .await?;
        vec![("uid", uid_str.as_str())]
    } else {
        vec![]
    };

    let filters = Filters::new(&[], &kv_pairs);

    // start multi-source log tracing
    let _tracer = Tracer::trace(sources, Duration::from_millis(200), filters).await;

    Print::info("Log tracer started. Waiting for events...")
        .render()
        .await?;

    pending::<()>().await;

    Ok(())
}
