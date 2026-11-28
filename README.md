<p align="center">
  <img src="https://raw.githubusercontent.com/fuderis/osy-kernel/main/assets/logo.png" alt="Logo" width="80" />
</p>

<h1 align="center">Osy Kernel</h1>
<p align="center">
  <strong>A lean AI orchestrator: one conversation, many isolated agents, minimum tokens.</strong><br>
  <code>rust</code> • <code>process-isolated agents</code> • <code>token-efficient</code>
</p>

<img src="https://raw.githubusercontent.com/fuderis/osy-kernel/main/assets/cover.png" alt="Cover" width="100%" />

**Osy** is a kernel for AI-powered toolkits. You talk to a single orchestrator; the actual work is done by
**agents**: separate processes that expose small, atomic tools. The orchestrator sees only short final results,
never raw JSON or tool logs, so context stays small and cost stays predictable.

Two goals drive every design decision:

* **Save tokens.** No tool traces in history, a compact dialog map instead of full transcripts, facts retrieved on demand,
  math in an embedded JS engine, and direct CLI calls that use no LLM at all.
* **Keep agents simple.** An agent is one binary and one `enum`. Transport, tool schemas, process lifecycle,
  dialogs and self-healing are the kernel's job.

> ⚠️ **Experimental.** IPC contracts and internals change quickly. LanceDB and Sled run inside the kernel process and may use noticeable RAM and I/O.

---

## How it works

<img src="https://raw.githubusercontent.com/fuderis/osy-kernel/main/assets/scheme.png" alt="Scheme" width="100%" />

* **Star topology.** Agents talk only to the kernel, never to each other, so there are no agent-to-agent loops.
* **Three ways to run**, from cheapest to richest:
  `osy skill.tool key=value` (no LLM) → `osy do skill "task"` (one isolated skill) → `osy chat` (full orchestrator).
* **Isolated skills.** A skill gets only the task text written by the orchestrator: no chat history. Its output returns as a short result.
* **Bounded loops.** Retries and iterations are capped; when automatic recovery runs out, the user decides via a dialog.
* **Interactive dialogs.** Agents can ask the user to confirm, type, pick, enter a secret, or run a script on the client machine.

### Memory

| Layer | Written by | Scope | In context |
|---|---|---|---|
| **Rules** | user | session or global | always |
| **Facts** | model or user | user, across sessions | hybrid search: meaning (cosine) + exact words |
| **Dialog map** | kernel | session | always, one truncated line per turn |
| **History** | kernel | session | last exchange + `read_history` on demand |

### Self-healing

Process crashes are repaired (agent restarted, request retried). Errors of tools, skills and JS become tool results, so the model
fixes the call itself and finished tasks are never re-run. Transport errors are retried with a pause within limits; the last step
runs without tools to report honestly. Only if no plan can be obtained at all, the user is asked whether to retry.
Details and known gaps: [docs/self-healing.md](docs/self-healing.md).

---

## Quickstart

**Requirements:** Rust (2024 edition), `jq` (used by `make build`), Linux / macOS / BSD.
Windows support is incomplete: agent management currently relies on Unix sockets.

```bash
git clone https://github.com/fuderis/osy-kernel.git osy && cd osy
make install-all
export ROUTERAI_API_KEY=...        # default provider; change it in `osy config`
```

```bash
osy chat                      # interactive chat with the orchestrator
osy do <skill> "your task"   # run one skill with a text task
osy test.hello name=Bob       # call a tool directly, no LLM
osy status                    # check the kernel and agents
```

Inside the chat: `/facts`, `/rules`, `/clear`, `/help`, `/exit`.

| Command | Purpose |
|---|---|
| `chat` | Interactive AI chat |
| `do` / `{skill} {payload..}` | Run a skill or tool |
| `agent list` / `agent new` | List agents / scaffold a new one |
| `server start\|stop\|restart\|status` | Kernel server control |
| `status`, `refresh`, `config`, `trace` | Health, reload, edit config, live logs |

## Writing an agent

```bash
osy agent new weather "Weather agent"
```

```rust
#[derive(AgentSkill, Clone, Copy, Debug, Display, Serialize, Deserialize, Eq, PartialEq, Hash)]
#[serde(rename_all = "snake_case")]
#[display(rename_all = "snake_case")]
pub enum SkillKind {
    /// Current weather and forecasts.
    #[tool(name = "now", description = "Weather for a city.")]
    Weather,
}
```

The macro generates the skill prompt, tool schemas and dispatch. You implement the handler and stream `Event`s back.

## Official agents

* **[osy-system](https://github.com/fuderis/osy-system.git)**: system metrics, appearance, power and media, disks.
* **[osy-forge](https://github.com/fuderis/osy-forge.git)**: development tools and Git.
* **[osy-home](https://github.com/fuderis/osy-home.git)**: smart home on Arduino Pro Micro (IR, relays, servos, motors).

## Ecosystem crates

`rigging` (terminal UI) • `anylm` (LLM and embeddings client) • `cistern` (Sled + LanceDB) • `pearce` (Axum, Unix sockets, SSE) • `atoman` (state, config, logging) • Boa (embedded JS).

## Documentation

Architecture, request algorithms, self-healing, memory and improvement proposals: **[docs/](docs/README.md)**.

## Roadmap

* [x] Isolated agents over Unix sockets, parallel skill execution
* [x] Long-term facts (RAG), rules, dialog map
* [x] Embedded JS runtime, interactive dialogs
* [x] Tool errors returned to the model, no duplicate retries, loop guard
* [ ] Session notes (the model's own scratchpad)
* [x] Fact retrieval without translation round trips; hybrid search (cosine + exact words)
* [ ] Token accounting and per-turn budgets
* [ ] Native web search

Contributions are welcome, especially in Rust systems code, IPC design and agent orchestration.

## License

[GNU GPL v3.0](LICENSE.md). A commercial license is available for use in proprietary products.
Contact: **Bulat Sharipov** ([@fuderis](https://github.com/fuderis) / `synapdrake@ya.ru`).
