//! 方法名与通知名的单一来源。

/// JSON-RPC 请求方法名。
pub mod method {
    /// 认证（建连后首个消息）。
    pub const AUTH: &str = "auth";
    /// 查询当前机器的 agents（名称与可用性）
    pub const AGENT_LIST: &str = "agent.list";
    /// 重启指定 agent
    pub const AGENT_RESTART: &str = "agent.restart";
    /// 新建一个普通会话（惰性：仅 server 侧写入，不触发 ACP）
    pub const SESSION_NEW: &str = "session.new";
    /// 往指定普通会话发送指令
    pub const SESSION_PROMPT: &str = "session.prompt";
    /// 取消指定普通会话正在进行的工作
    pub const SESSION_CANCEL: &str = "session.cancel";
    /// 删除指定普通会话
    pub const SESSION_DELETE: &str = "session.delete";
    /// 配置指定普通会话（会话标题）
    pub const SESSION_CONFIGURE: &str = "session.configure";
    /// 设置指定普通会话的配置选项（ACP session/set_config_option；选项由 ACP 会话提供）
    pub const SESSION_SET_CONFIG_OPTION: &str = "session.set_config_option";
    /// 分页查询指定普通会话的对话历史
    pub const SESSION_HISTORY: &str = "session.history";
    /// 分页查询指定普通会话的活动历史
    pub const SESSION_ACTIVITIES: &str = "session.activities";
    /// 查询指定普通会话正在进行中的活动
    pub const SESSION_ONGOING_ACTIVITY: &str = "session.ongoing_activity";
    /// 分页查询普通会话列表
    pub const SESSION_LIST: &str = "session.list";
    /// 批量查询指定的普通会话列表
    pub const SESSION_INFO: &str = "session.info";
    /// 查询普通会话工作目录改动 diff
    pub const WORKSPACE_DIFF: &str = "workspace.diff";
    /// 按文件或代码块撤销普通会话工作目录的改动
    pub const WORKSPACE_RESTORE: &str = "workspace.restore";
    /// 分页查看工作目录指定文件夹内容
    pub const WORKSPACE_LIST: &str = "workspace.list";
    /// 分页读取工作目录文本文件内容
    pub const WORKSPACE_READ: &str = "workspace.read";
}

/// server → GUI 通知名。
pub mod notify {
    /// 普通会话状态变更事件（工作流驱动等依赖它）
    pub const SESSION_STATE_CHANGE: &str = "session.state_change";
}
