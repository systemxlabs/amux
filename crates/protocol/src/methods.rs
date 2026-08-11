//! 方法名与通知名（协议面，单一来源）。

/// JSON-RPC 请求方法名。
pub mod method {
    /// 机器信息（server 版本 + harness 列表）
    pub const GET_INFO: &str = "get_info";
    /// 会话列表
    pub const LIST_SESSIONS: &str = "list_sessions";
    /// 新建会话
    pub const CREATE_SESSION: &str = "create_session";
    /// 删除会话（历史一并移除）
    pub const DELETE_SESSION: &str = "delete_session";
    /// 发送 prompt（idle 启动新工作、忙时 steer）
    pub const PROMPT: &str = "prompt";
    /// 取消进行中的工作
    pub const CANCEL: &str = "cancel";
    /// 打开会话：server 经 ACP `session/load` 全量重放，聚合对话内容返回
    pub const OPEN_SESSION: &str = "open_session";
    /// 获取会话活动（activities，server 有界缓存）
    pub const GET_ACTIVITIES: &str = "get_activities";
    /// git status（cwd 非 git 仓库时返回 not_repo 标记）
    pub const GIT_STATUS: &str = "git_status";
    /// git diff
    pub const GIT_DIFF: &str = "git_diff";
    /// git push
    pub const GIT_PUSH: &str = "git_push";
    /// git revert（undo；需工作区间结束）
    pub const GIT_REVERT: &str = "git_revert";
    /// 修改会话标题（用户可随时修改）
    pub const SET_SESSION_TITLE: &str = "set_session_title";
    /// 配置 agent 默认模型（server 侧持久化）
    pub const SET_DEFAULT_MODEL: &str = "set_default_model";
    /// 查询某 agent 安装的 skills 列表
    pub const LIST_AGENT_SKILLS: &str = "list_agent_skills";
}

/// server → GUI 通知名。
pub mod notify {
    /// 会话创建
    pub const SESSION_CREATED: &str = "session_created";
    /// 会话中断（崩溃恢复标记）
    pub const SESSION_INTERRUPTED: &str = "session_interrupted";
    /// 会话删除
    pub const SESSION_DELETED: &str = "session_deleted";
    /// 会话元数据更新（标题修改等）
    pub const SESSION_UPDATED: &str = "session_updated";
    /// turn 完成：agent 完整输出（非流式交付，docs/DESIGN.md §5.1）
    pub const TURN_COMPLETED: &str = "turn_completed";
    /// 会话状态（进行中 / 完成，turn 边界）
    pub const SESSION_STATE: &str = "session_state";
    /// 用户消息（GUI 本地立即渲染，可经此同步）
    pub const USER_MESSAGE: &str = "user_message";
    /// 实时活动（turn 中经 ACP 事件流合并的当前活动，thinking 逐块累积流式推送）
    pub const ACTIVITY: &str = "activity";
}
