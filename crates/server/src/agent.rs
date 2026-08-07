//! Agent 驱动抽象（docs/DESIGN.md §9）：server 与 agent harness 的唯一接口。
//! 真实实现经 ACP v1（stdio，`codex-acp` / `claude-acp` / `kimi acp`）；
//! 本模块提供 trait 与内存 Stub（测试/演示用）。

use std::sync::Arc;

use tokio::sync::mpsc;

use protocol::ContentBlock;

/// turn 过程中的 agent 事件（server 聚合为输出 + activities，docs/DESIGN.md §5）。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// agent 输出的增量片段（聚合为完整输出，非流式交付）
    OutputChunk(String),
    /// 思考活动
    Thinking(String),
    /// 工具调用活动
    ToolCall {
        name: String,
        title: Option<String>,
        content: Option<String>,
    },
    /// 上下文压缩活动（ACP 场景产生；Stub 暂不产生）
    #[allow(dead_code)]
    Compaction(String),
    /// turn 完成
    TurnEnded,
}

/// 与单个 agent harness 的驱动接口（ACP v1 语义的投影）。
pub trait AgentDriver: Send + Sync {
    /// 新建会话，返回 agent 侧会话 id
    fn create_session(&self, cwd: &str, model: Option<&str>) -> Result<String, String>;
    /// 加载会话（ACP `session/load` 全量重放；返回对话内容）
    fn load_session(&self, agent_session_id: &str) -> Result<Vec<DialogRecord>, String>;
    /// 恢复会话上下文
    fn resume_session(&self, agent_session_id: &str) -> Result<(), String>;
    /// 发送 prompt，返回事件流（阻塞直到 turn 结束）
    fn prompt(
        &self,
        agent_session_id: &str,
        input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent>;
    /// 取消进行中的工作
    fn cancel(&self, agent_session_id: &str) -> Result<(), String>;
    /// 关闭会话（保留历史可恢复）
    fn close(&self, agent_session_id: &str) -> Result<(), String>;
    /// 删除会话（历史一并移除）
    fn delete(&self, agent_session_id: &str) -> Result<(), String>;
    /// 列出 agent 侧全部会话 id（server 重启后从 agent 恢复会话列表，docs/DESIGN.md §3）
    #[allow(dead_code)]
    fn list_sessions(&self) -> Vec<String>;
}

/// 对话内容条目（load 重放的产物）。
#[derive(Debug, Clone)]
pub enum DialogRecord {
    #[allow(dead_code)]
    UserMessage(Vec<ContentBlock>),
    #[allow(dead_code)]
    AgentOutput(Vec<ContentBlock>),
}

pub type SharedDriver = Arc<dyn AgentDriver>;

// ---- 内存 Stub（测试/演示；模拟一个 turn 的事件流）----

pub struct StubAgentDriver {
    sessions: std::sync::Mutex<Vec<String>>,
    pub output_prefix: String,
}

impl StubAgentDriver {
    pub fn new() -> Self {
        StubAgentDriver {
            sessions: std::sync::Mutex::new(Vec::new()),
            output_prefix: "模拟输出：".into(),
        }
    }
}

impl AgentDriver for StubAgentDriver {
    fn create_session(&self, cwd: &str, _model: Option<&str>) -> Result<String, String> {
        let id = format!("agent_{}", cwd.replace('/', "_"));
        self.sessions.lock().unwrap().push(id.clone());
        Ok(id)
    }

    fn load_session(&self, _agent_session_id: &str) -> Result<Vec<DialogRecord>, String> {
        Ok(Vec::new()) // stub 无持久化历史
    }

    fn resume_session(&self, _agent_session_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn prompt(
        &self,
        _agent_session_id: &str,
        _input: Vec<ContentBlock>,
    ) -> mpsc::Receiver<AgentEvent> {
        let (tx, rx) = mpsc::channel(16);
        let prefix = self.output_prefix.clone();
        tokio::spawn(async move {
            tx.send(AgentEvent::Thinking("正在分析问题…".into()))
                .await
                .ok();
            tx.send(AgentEvent::ToolCall {
                name: "read_file".into(),
                title: Some("读取 src/main.rs".into()),
                content: None,
            })
            .await
            .ok();
            tx.send(AgentEvent::OutputChunk(format!("{prefix}完成")))
                .await
                .ok();
            tx.send(AgentEvent::TurnEnded).await.ok();
        });
        rx
    }

    fn cancel(&self, _agent_session_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn close(&self, _agent_session_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn delete(&self, agent_session_id: &str) -> Result<(), String> {
        self.sessions
            .lock()
            .unwrap()
            .retain(|s| s != agent_session_id);
        Ok(())
    }

    fn list_sessions(&self) -> Vec<String> {
        self.sessions.lock().unwrap().clone()
    }
}
