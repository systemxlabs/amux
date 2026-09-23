//! Server 运行态：存储、配置、机器接入、会话与工作流服务。

use std::sync::Arc;

use crate::attachments::AttachmentStore;
use crate::config_store::ConfigStore;
use crate::machines::MachineHub;
use crate::sessions::SessionService;
use crate::workflows::WorkflowService;

pub struct AppState {
    pub token: String,
    pub config: Arc<ConfigStore>,
    pub machines: MachineHub,
    pub sessions: Arc<SessionService>,
    pub workflows: Arc<WorkflowService>,
    pub attachments: Arc<AttachmentStore>,
    pub public_url: Option<String>,
}
