//! 会话附件存储：文件位于 `~/.amux/{sessions|workflows}/<会话 ID>/attachments/`。

use std::fs;
use std::path::{Path, PathBuf};

use amux_common::api::{Attachment, AttachmentList};
use uuid::Uuid;

use crate::timestamps::now_ms;

/// 单个附件上限。
pub const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum AttachmentOwner<'a> {
    Session(&'a str),
    Workflow(&'a str),
}

impl<'a> AttachmentOwner<'a> {
    fn kind(self) -> &'static str {
        match self {
            Self::Session(_) => "sessions",
            Self::Workflow(_) => "workflows",
        }
    }

    fn id(self) -> &'a str {
        match self {
            Self::Session(id) | Self::Workflow(id) => id,
        }
    }
}

pub struct AttachmentStore {
    home: PathBuf,
}

impl AttachmentStore {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn save(
        &self,
        owner: AttachmentOwner<'_>,
        original_name: &str,
        bytes: &[u8],
        public_url: &str,
    ) -> Result<Attachment, String> {
        if bytes.is_empty() {
            return Err("附件内容为空".into());
        }
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err("单个附件不得超过 20 MB".into());
        }
        let extension = Path::new(original_name)
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| {
                !extension.is_empty()
                    && extension.len() <= 16
                    && extension
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric())
            });
        let name = match extension {
            Some(extension) => format!("{}.{}", Uuid::new_v4(), extension),
            None => Uuid::new_v4().to_string(),
        };
        let dir = self.directory(owner);
        fs::create_dir_all(&dir).map_err(|error| format!("创建附件目录失败: {error}"))?;
        fs::write(dir.join(&name), bytes).map_err(|error| format!("写入附件失败: {error}"))?;
        let created_at = now_ms();
        Ok(Attachment {
            name: name.clone(),
            uri: public_uri(public_url, owner, &name),
            size: bytes.len() as u64,
            created_at,
        })
    }

    pub fn list(
        &self,
        owner: AttachmentOwner<'_>,
        limit: usize,
        offset: usize,
        public_url: Option<&str>,
    ) -> Result<AttachmentList, String> {
        let dir = self.directory(owner);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AttachmentList {
                    attachments: Vec::new(),
                    has_more: false,
                });
            }
            Err(error) => return Err(format!("读取附件目录失败: {error}")),
        };
        let mut attachments = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| format!("读取附件失败: {error}"))?;
            if !entry
                .file_type()
                .map_err(|error| format!("读取附件类型失败: {error}"))?
                .is_file()
            {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = entry
                .metadata()
                .map_err(|error| format!("读取附件元信息失败: {error}"))?;
            let created_at = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_millis() as u64);
            attachments.push(Attachment {
                uri: public_url
                    .map(|base| public_uri(base, owner, &name))
                    .unwrap_or_default(),
                name,
                size: metadata.len(),
                created_at,
            });
        }
        attachments.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.name.cmp(&right.name))
        });
        let has_more = attachments.len() > offset.saturating_add(limit);
        Ok(AttachmentList {
            attachments: attachments.into_iter().skip(offset).take(limit).collect(),
            has_more,
        })
    }

    pub fn read(&self, owner: AttachmentOwner<'_>, name: &str) -> Result<Vec<u8>, String> {
        let path = self.attachment_path(owner, name)?;
        fs::read(path).map_err(|error| format!("读取附件失败: {error}"))
    }

    pub fn delete(&self, owner: AttachmentOwner<'_>, name: &str) -> Result<(), String> {
        let path = self.attachment_path(owner, name)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("删除附件失败: {error}")),
        }
    }

    pub fn delete_all(&self, owner: AttachmentOwner<'_>) -> Result<(), String> {
        let dir = self.directory(owner);
        match fs::remove_dir_all(dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("删除附件失败: {error}")),
        }
    }

    fn directory(&self, owner: AttachmentOwner<'_>) -> PathBuf {
        self.home
            .join(owner.kind())
            .join(owner.id())
            .join("attachments")
    }

    fn attachment_path(&self, owner: AttachmentOwner<'_>, name: &str) -> Result<PathBuf, String> {
        if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name))
            || name == "."
            || name == ".."
        {
            return Err("附件名称无效".into());
        }
        Ok(self.directory(owner).join(name))
    }
}

fn public_uri(public_url: &str, owner: AttachmentOwner<'_>, name: &str) -> String {
    format!(
        "{}/{}/{}/attachments/{}",
        public_url.trim_end_matches('/'),
        owner.kind(),
        owner.id(),
        name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachments_are_stored_listed_read_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(dir.path().to_path_buf());
        let owner = AttachmentOwner::Session("session-1");

        let saved = store
            .save(owner, "report.PDF", b"hello", "https://amux.example.com/")
            .unwrap();
        assert!(saved.name.ends_with(".PDF"));
        assert_eq!(
            saved.uri,
            format!(
                "https://amux.example.com/sessions/session-1/attachments/{}",
                saved.name
            )
        );
        assert_eq!(saved.size, 5);

        let page = store
            .list(owner, 50, 0, Some("https://amux.example.com"))
            .unwrap();
        assert_eq!(page.attachments.len(), 1);
        assert_eq!(page.attachments[0].name, saved.name);
        assert_eq!(page.attachments[0].size, saved.size);
        assert_eq!(page.attachments[0].uri, saved.uri);
        assert!(!page.has_more);
        assert_eq!(store.read(owner, &saved.name).unwrap(), b"hello");

        store.delete(owner, &saved.name).unwrap();
        assert!(store
            .list(owner, 50, 0, None)
            .unwrap()
            .attachments
            .is_empty());
    }

    #[test]
    fn attachment_limits_and_names_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(dir.path().to_path_buf());
        let owner = AttachmentOwner::Workflow("workflow-1");

        assert!(store
            .save(
                owner,
                "large.bin",
                &vec![0; MAX_ATTACHMENT_BYTES + 1],
                "https://amux.example.com"
            )
            .is_err());
        let saved = store
            .save(owner, "plain", b"x", "https://amux.example.com")
            .unwrap();
        assert!(!saved.name.contains('.'));
        assert!(store.read(owner, "../plain").is_err());
    }
}
