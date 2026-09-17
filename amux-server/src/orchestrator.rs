//! 工作流智能体：使用共享的非流式模型工具循环。

use amux_common::api::OrchestratorConfig;
pub use amux_common::model::{ToolFuture, Tools};
use rig_core::completion::Message;

pub async fn run(
    config: &OrchestratorConfig,
    preamble: &str,
    mut history: Vec<Message>,
    tools: &dyn Tools,
) -> Result<String, String> {
    amux_common::model::run(config, preamble, &mut history, tools).await
}

pub fn preamble(plan: &str) -> String {
    format!(
        "你是 amux 的工作流智能体。按工作流计划与用户指令调度，传递用户指令和关联普通会话内容。\n\
         不进行任务拆解、任务执行和任务决策；可执行计划中明确写出的条件分支，但不创造计划之外的步骤、不自主变更目标。\n\
         未在计划与用户指令中指定的事项交由用户决定；需要人类判断时输出结论并等待用户指令。\n\
         只能操作本工作流的关联普通会话。\n\n\
         工作流计划：\n{plan}"
    )
}
