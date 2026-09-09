//! 方法名与通知名的单一来源。

/// JSON-RPC 请求方法名。
pub mod method {
    /// 认证（建连后首个消息）。
    pub const AUTH: &str = "auth";
    pub const AGENT_LIST: &str = "agent.list";
    pub const AGENT_RESTART: &str = "agent.restart";
    /// 重扫本机 ACP agent 并拉起未运行的实例。
    pub const AGENT_REDISCOVER: &str = "agent.rediscover";
    /// 只写入 Server 侧注册表，不立即创建 agent 侧会话。
    pub const SESSION_NEW: &str = "session.new";
    pub const SESSION_PROMPT: &str = "session.prompt";
    pub const SESSION_CANCEL: &str = "session.cancel";
    pub const SESSION_DELETE: &str = "session.delete";
    pub const SESSION_CONFIGURE: &str = "session.configure";
    /// 查询会话选项会触发 agent 侧会话的惰性创建或恢复。
    pub const SESSION_CONFIG_OPTIONS: &str = "session.config_options";
    /// 结果来自 ACP `available_commands_update` 缓存，查询本身不创建会话。
    pub const SESSION_SLASH_COMMANDS: &str = "session.slash_commands";
    /// 结果来自 ACP `plan` 通知缓存，查询本身不创建会话。
    pub const SESSION_PLAN: &str = "session.plan";
    /// 结果来自 ACP `usage_update` 通知的内存缓存，查询本身不创建会话。
    pub const SESSION_CONTEXT: &str = "session.context";
    pub const SESSION_HISTORY: &str = "session.history";
    pub const SESSION_ACTIVITIES: &str = "session.activities";
    pub const SESSION_ONGOING_ACTIVITY: &str = "session.ongoing_activity";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_INFO: &str = "session.info";
    pub const WORKSPACE_DIFF: &str = "workspace.diff";
    pub const WORKSPACE_RESTORE: &str = "workspace.restore";
    /// 分页列出指定绝对路径目录下的条目（不限于会话工作目录）。
    pub const FS_LIST: &str = "fs.list";
    /// 分页读取指定绝对路径文本文件内容。
    pub const FS_READ: &str = "fs.read";
    /// 指定初始行列打开终端，避免全屏程序先以默认尺寸渲染。
    pub const TERMINAL_OPEN: &str = "terminal.open";
    pub const TERMINAL_RESIZE: &str = "terminal.resize";
    pub const TERMINAL_INPUT: &str = "terminal.input";
    pub const TERMINAL_CLOSE: &str = "terminal.close";
}

/// server → GUI 通知名。
pub mod notify {
    /// 普通会话状态变更事件（工作流驱动等依赖它）
    pub const SESSION_STATE_CHANGE: &str = "session.state_change";
    /// 终端输出事件（PTY 字节流，base64）。仅推送给该终端所属的应用连接，
    /// 不进入 `session.state_change` 的全局广播流
    pub const TERMINAL_OUTPUT: &str = "terminal.output";
    /// 终端进程退出事件（shell 敲 exit 或异常终止；客户端据此清理 UI 状态）
    pub const TERMINAL_EXIT: &str = "terminal.exit";
}
