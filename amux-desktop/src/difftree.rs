//! 改动文件树：按目录层级组织改动文件（docs/PRD.md「文件改动审查视图」）。
//!
//! 只有含改动文件的目录出现在树中；不含改动文件的中间目录与其唯一子目录合并为一个节点
//! （如 `storage/s3`），合并节点只能整体折叠/展开，因此树已把合并结果固定在节点上。

use amux_common::domain::GitDiffFile;

/// 改动文件树节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffNode {
    /// 节点展示名；合并目录为 `a/b` 形式
    pub name: String,
    /// 节点键（完整路径，折叠状态按它记忆）
    pub key: String,
    /// 目录子节点（文件为空）
    pub children: Vec<DiffNode>,
    /// 文件在改动列表中的下标（目录为 None）
    pub file_ix: Option<usize>,
}

/// 由改动文件列表构建树：目录在前、文件在后，各自保持原有顺序。
pub fn build(files: &[GitDiffFile]) -> Vec<DiffNode> {
    let mut root = Dir::default();
    for (ix, file) in files.iter().enumerate() {
        insert(&mut root, &file.path, ix);
    }
    to_nodes(&root, "")
}

#[derive(Default)]
struct Dir {
    name: String,
    dirs: Vec<Dir>,
    files: Vec<(String, usize)>,
}

fn insert(dir: &mut Dir, path: &str, ix: usize) {
    match path.split_once('/') {
        Some((head, rest)) => {
            let child = match dir.dirs.iter().position(|child| child.name == head) {
                Some(ix) => &mut dir.dirs[ix],
                None => {
                    dir.dirs.push(Dir {
                        name: head.to_string(),
                        ..Default::default()
                    });
                    dir.dirs.last_mut().expect("刚刚压入")
                }
            };
            insert(child, rest, ix);
        }
        None => dir.files.push((path.to_string(), ix)),
    }
}

fn to_nodes(dir: &Dir, prefix: &str) -> Vec<DiffNode> {
    let mut nodes: Vec<DiffNode> = dir
        .dirs
        .iter()
        .map(|child| dir_node(child, prefix))
        .collect();
    nodes.extend(dir.files.iter().map(|(name, ix)| DiffNode {
        name: name.clone(),
        key: join(prefix, name),
        children: Vec::new(),
        file_ix: Some(*ix),
    }));
    nodes
}

/// 目录节点：向下吞并唯一子目录直到分支点，形成一个合并节点。
fn dir_node(dir: &Dir, prefix: &str) -> DiffNode {
    let mut name = dir.name.clone();
    let mut current = dir;
    while current.files.is_empty() && current.dirs.len() == 1 {
        current = &current.dirs[0];
        name = format!("{name}/{}", current.name);
    }
    let key = join(prefix, &name);
    DiffNode {
        name,
        children: to_nodes(current, &key),
        key,
        file_ix: None,
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use amux_common::domain::{GitChangeStatus, GitDiffFile};

    use super::*;

    fn file(path: &str) -> GitDiffFile {
        GitDiffFile {
            path: path.to_string(),
            status: GitChangeStatus::Modified,
            additions: 1,
            deletions: 1,
            patch: String::new(),
            hunks: Vec::new(),
        }
    }

    /// 只有唯一子目录的中间目录合并为一个节点（三例来自 PRD 的示例树）。
    #[test]
    fn merges_directories_without_branches() {
        let files = [
            file("src/catalog/helper/query.rs"),
            file("src/catalog/schema.rs"),
            file("src/storage/s3/parquet.rs"),
        ];
        let tree = build(&files);

        // 深度优先：storage 与 s3 合并为一个节点
        assert_eq!(
            keys(&tree),
            [
                "src",
                "src/catalog",
                "src/catalog/helper",
                "src/catalog/helper/query.rs",
                "src/catalog/schema.rs",
                "src/storage/s3",
                "src/storage/s3/parquet.rs",
            ]
        );
        let src = &tree[0];
        assert_eq!(
            src.children
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["catalog", "storage/s3"]
        );
        // 分支点不再合并：catalog 下有目录 helper 与文件 schema.rs
        let catalog = &src.children[0];
        assert_eq!(
            catalog
                .children
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["helper", "schema.rs"]
        );
        assert_eq!(catalog.children[1].file_ix, Some(1));
        assert_eq!(catalog.children[0].children[0].file_ix, Some(0));
    }

    /// 根目录下的文件与目录同级，文件带改动列表下标。
    #[test]
    fn keeps_root_files_and_indices() {
        let files = [file("Cargo.toml"), file("src/main.rs")];
        let tree = build(&files);
        assert_eq!(keys(&tree), ["src", "src/main.rs", "Cargo.toml"]);
        assert_eq!(tree[1].file_ix, Some(0));
    }

    /// 深度优先展开所有节点键（含合并节点与其子节点）。
    fn keys(nodes: &[DiffNode]) -> Vec<String> {
        let mut collected = Vec::new();
        for node in nodes {
            collected.push(node.key.clone());
            collected.extend(keys(&node.children));
        }
        collected
    }
}
