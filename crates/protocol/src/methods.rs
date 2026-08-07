//! 方法名与通知名（协议面，单一来源）。

/// JSON-RPC 请求方法名。
pub mod method {
    /// 机器信息（server 版本 + harness 列表）
    pub const GET_INFO: &str = "get_info";
    /// 会话列表
    pub const LIST_SESSIONS: &str = "list_sessions";
    /// 新建会话
    pub const CREATE_SESSION: &str = "create_session";
    /// 恢复会话
    pub const RESUME_SESSION: &str = "resume_session";
    /// 关闭会话（保留历史，可恢复）
    pub const CLOSE_SESSION: &str = "close_session";
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
}

/// server → GUI 通知名。
pub mod notify {
    /// 会话创建
    pub const SESSION_CREATED: &str = "session_created";
    /// 会话关闭
    pub const SESSION_CLOSED: &str = "session_closed";
    /// 会话中断（崩溃恢复标记）
    pub const SESSION_INTERRUPTED: &str = "session_interrupted";
    /// 会话删除
    pub const SESSION_DELETED: &str = "session_deleted";
    /// turn 完成：agent 完整输出（非流式交付，docs/DESIGN.md §5.1）
    pub const TURN_COMPLETED: &str = "turn_completed";
    /// 会话状态（进行中 / 完成，turn 边界）
    pub const SESSION_STATE: &str = "session_state";
    /// 用户消息（GUI 本地立即渲染，可经此同步）
    pub const USER_MESSAGE: &str = "user_message";
}
