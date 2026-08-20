//! 方法名与通知名（协议面，单一来源）。

/// JSON-RPC 请求方法名。
pub mod method {
    pub const AUTH: &str = "auth";
    pub const AGENT_LIST: &str = "agent.list";
    pub const AGENT_RESTART: &str = "agent.restart";
    pub const AGENT_SKILLS: &str = "agent.skills";
    pub const SESSION_NEW: &str = "session.new";
    pub const SESSION_PROMPT: &str = "session.prompt";
    pub const SESSION_CANCEL: &str = "session.cancel";
    pub const SESSION_DELETE: &str = "session.delete";
    pub const SESSION_CONFIGURE: &str = "session.configure";
    pub const SESSION_HISTORY: &str = "session.history";
    pub const SESSION_ACTIVITIES: &str = "session.activities";
    pub const SESSION_ONGOING_ACTIVITY: &str = "session.ongoing_activity";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_INFO: &str = "session.info";
    pub const WORKSPACE_DIFF: &str = "workspace.diff";
    pub const WORKSPACE_RESTORE: &str = "workspace.restore";

    // Canonical aliases used by the existing client implementation.
    pub const GET_INFO: &str = AGENT_LIST;
    pub const LIST_SESSIONS: &str = SESSION_LIST;
    pub const CREATE_SESSION: &str = SESSION_NEW;
    pub const DELETE_SESSION: &str = SESSION_DELETE;
    pub const PROMPT: &str = SESSION_PROMPT;
    pub const CANCEL: &str = SESSION_CANCEL;
    pub const OPEN_SESSION: &str = SESSION_HISTORY;
    pub const GIT_STATUS: &str = "workspace.status";
    pub const GIT_DIFF: &str = WORKSPACE_DIFF;
    pub const GIT_PUSH: &str = "workspace.push";
    pub const GIT_REVERT: &str = WORKSPACE_RESTORE;
    pub const SET_SESSION_TITLE: &str = SESSION_CONFIGURE;
    pub const SET_DEFAULT_MODEL: &str = "agent.configure";
    pub const LIST_AGENT_SKILLS: &str = AGENT_SKILLS;
    pub const RETRY_HARNESS: &str = AGENT_RESTART;

    // Accepted only for clients from the pre-Design protocol. New clients must
    // use the dotted names above.
    pub const LEGACY_GET_INFO: &str = "get_info";
    pub const LEGACY_LIST_SESSIONS: &str = "list_sessions";
    pub const LEGACY_CREATE_SESSION: &str = "create_session";
    pub const LEGACY_DELETE_SESSION: &str = "delete_session";
    pub const LEGACY_PROMPT: &str = "prompt";
    pub const LEGACY_CANCEL: &str = "cancel";
    pub const LEGACY_OPEN_SESSION: &str = "open_session";
    pub const LEGACY_GIT_STATUS: &str = "git_status";
    pub const LEGACY_GIT_DIFF: &str = "git_diff";
    pub const LEGACY_GIT_PUSH: &str = "git_push";
    pub const LEGACY_GIT_REVERT: &str = "git_revert";
    pub const LEGACY_SET_SESSION_TITLE: &str = "set_session_title";
    pub const LEGACY_SET_DEFAULT_MODEL: &str = "set_default_model";
    pub const LEGACY_LIST_AGENT_SKILLS: &str = "list_agent_skills";
    pub const LEGACY_RETRY_HARNESS: &str = "retry_harness";
}

/// server → GUI 通知名。
pub mod notify {
    pub const SESSION_STATE_CHANGE: &str = "session.state_change";
    pub const SESSION_CREATED: &str = "session_created";
    pub const SESSION_INTERRUPTED: &str = "session_interrupted";
    pub const SESSION_DELETED: &str = "session_deleted";
    pub const SESSION_UPDATED: &str = "session_updated";
    pub const PASSTHROUGH: &str = "passthrough";
}
