//! Agent 发现、生命周期和 ACP 消息转发。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use amux_common::daemon::{notify, AcpForward, AgentListResult, DiscoveredAgent};
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::frames;
use crate::outbox::Outbox;

/// 已知 agent 的启动方式。
struct LaunchSpec {
    program: &'static str,
    args: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
}

const KNOWN_AGENTS: &[&str] = &[amux_common::api::NANO_AGENT, "codex"];

fn launch_spec(agent: &str) -> Option<LaunchSpec> {
    match agent {
        "codex" => Some(LaunchSpec {
            program: "npx",
            args: &["-y", "@nyssance/codex-acp-v2"],
            env: &[("INITIAL_AGENT_MODE", "agent-full-access")],
        }),
        _ => None,
    }
}

/// 本机是否已安装该 agent。
fn is_installed(agent: &str) -> bool {
    match agent {
        amux_common::api::NANO_AGENT => true,
        // codex 经 npx 启动：需要 codex CLI、npx 与 bun 同时可用
        "codex" => in_path("codex") && in_path("npx") && in_path("bun"),
        _ => false,
    }
}

fn in_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// 一个已启动的 agent 实例。
struct RunningAgent {
    /// 实例号：重启后旧实例的退出回调不得影响新实例
    instance: u64,
    process: AgentProcess,
    /// ACP 下行写入端（Server → Agent stdin）
    stdin: mpsc::Sender<String>,
}

enum AgentProcess {
    External(u32),
    Nano(tokio::task::JoinHandle<()>),
}

pub struct AgentRegistry {
    agents: Mutex<HashMap<String, RunningAgent>>,
    next_instance: std::sync::atomic::AtomicU64,
}

impl AgentRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            agents: Mutex::new(HashMap::new()),
            next_instance: std::sync::atomic::AtomicU64::new(1),
        })
    }

    /// 发现本机已安装的 agents 及各自是否已启动。
    pub fn list(&self) -> AgentListResult {
        let running = self.agents.lock();
        AgentListResult {
            agents: KNOWN_AGENTS
                .iter()
                .filter(|agent| is_installed(agent))
                .map(|agent| DiscoveredAgent {
                    name: (*agent).to_string(),
                    running: running.contains_key(*agent),
                })
                .collect(),
        }
    }

    /// 启动（未启动）或重启（已启动）指定 agent。
    pub async fn restart(self: &Arc<Self>, agent: &str, outbox: Arc<Outbox>) -> Result<(), String> {
        if !is_installed(agent) {
            return Err(format!("本机未安装 agent: {agent}"));
        }
        self.terminate(agent).await;
        let running = if agent == amux_common::api::NANO_AGENT {
            let (stdin, incoming) = mpsc::channel(256);
            let instance = self
                .next_instance
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let task = tokio::spawn(async move {
                if let Err(error) = crate::nano::run(incoming, outbox).await {
                    log::warn!("Nano 已停止: {error}");
                }
            });
            RunningAgent {
                instance,
                process: AgentProcess::Nano(task),
                stdin,
            }
        } else {
            let spec = launch_spec(agent).ok_or_else(|| format!("未知 agent: {agent}"))?;
            self.spawn(agent, spec, outbox).await?
        };
        log::info!("agent 已启动: {agent}");
        self.agents.lock().insert(agent.to_string(), running);
        Ok(())
    }

    /// Server 下行的 ACP 消息写入对应 agent 的 stdin。
    pub fn forward_to_agent(&self, agent: &str, raw: &str) -> Result<(), String> {
        let agents = self.agents.lock();
        let running = agents
            .get(agent)
            .ok_or_else(|| format!("agent 未启动: {agent}"))?;
        running
            .stdin
            .try_send(raw.to_string())
            .map_err(|error| format!("agent 写入失败 ({agent}): {error}"))
    }

    /// 关闭全部 agent（Daemon 退出时调用）。
    pub async fn shutdown_all(&self) {
        let names: Vec<String> = self.agents.lock().keys().cloned().collect();
        for name in names {
            self.terminate(&name).await;
        }
    }

    /// 终止并移除指定 agent；未启动时为空操作。
    async fn terminate(&self, agent: &str) {
        let running = self.agents.lock().remove(agent);
        if let Some(running) = running {
            match running.process {
                AgentProcess::External(pid) => terminate_process_group(pid).await,
                AgentProcess::Nano(task) => {
                    task.abort();
                    let _ = task.await;
                }
            }
            log::info!("agent 已停止: {agent}");
        }
    }

    async fn spawn(
        self: &Arc<Self>,
        agent: &str,
        spec: LaunchSpec,
        outbox: Arc<Outbox>,
    ) -> Result<RunningAgent, String> {
        let mut command = Command::new(spec.program);
        command.args(spec.args);
        for (key, value) in spec.env {
            command.env(key, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 独立进程组：npx 包装器会再拉起 node，重启时需整组回收
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|error| format!("启动 agent 失败 ({agent}): {error}"))?;
        let pid = child
            .id()
            .ok_or_else(|| format!("agent 缺少 pid: {agent}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("agent stdin 不可用: {agent}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("agent stdout 不可用: {agent}"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| format!("agent stderr 不可用: {agent}"))?;

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(256);
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(line) = stdin_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let agent_name = agent.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let forward = AcpForward {
                    agent: agent_name.clone(),
                    raw: line,
                };
                outbox.push(frames::notification(notify::ACP, &forward));
            }
            log::info!("agent stdout 结束: {agent_name}");
        });

        let agent_name = agent.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log::debug!("[{agent_name}] {line}");
            }
        });

        let instance = self
            .next_instance
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let registry = Arc::clone(self);
        let agent_name = agent.to_string();
        tokio::spawn(async move {
            let status = child.wait().await;
            registry.on_exit(&agent_name, instance, status);
        });

        Ok(RunningAgent {
            instance,
            process: AgentProcess::External(pid),
            stdin: stdin_tx,
        })
    }

    /// Agent 进程退出：清除登记（仅当仍是同一实例）。
    fn on_exit(
        &self,
        agent: &str,
        instance: u64,
        status: std::io::Result<std::process::ExitStatus>,
    ) {
        let mut agents = self.agents.lock();
        if agents.get(agent).map(|running| running.instance) == Some(instance) {
            agents.remove(agent);
            log::warn!("agent 进程退出: {agent} ({status:?})");
        }
    }
}

/// 终止整个进程组（SIGTERM，宽限期后 SIGKILL）。
async fn terminate_process_group(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: 只是向指定进程组发送信号；pid 来自本进程 fork 出的子进程。
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGTERM);
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_agent_has_no_launch_spec() {
        assert!(launch_spec("unknown").is_none());
        assert!(launch_spec("codex").is_some());
    }

    #[test]
    fn list_only_reports_installed_agents() {
        let registry = AgentRegistry::new();
        let list = registry.list();
        for agent in list.agents {
            assert!(KNOWN_AGENTS.contains(&agent.name.as_str()));
            assert!(!agent.running, "未启动时 running 应为 false");
        }
    }

    #[test]
    fn forward_to_agent_requires_running_agent() {
        let registry = AgentRegistry::new();
        let error = registry.forward_to_agent("codex", "{}").unwrap_err();
        assert!(error.contains("未启动"), "{error}");
    }
}
