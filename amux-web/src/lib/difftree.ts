// 改动文件树：按目录层级组织改动文件（docs/PRD.md「改动审查视图」）。
//
// 只有含改动文件的目录出现在树中；不含改动文件的中间目录与其唯一子目录合并为一个节点
// （如 `storage/s3`），合并节点只能整体折叠/展开，因此树已把合并结果固定在节点上。

import type { GitDiffFile } from "./types";

/** 改动文件树节点。 */
export type DiffNode = {
  /** 节点展示名；合并目录为 `a/b` 形式 */
  name: string;
  /** 节点键（完整路径，折叠状态按它记忆） */
  key: string;
  /** 目录子节点（文件为空） */
  children: DiffNode[];
  /** 文件在改动列表中的下标（目录为 null） */
  fileIx: number | null;
};

type Dir = { name: string; dirs: Dir[]; files: { name: string; ix: number }[] };

function insert(dir: Dir, path: string, ix: number): void {
  const index = path.indexOf("/");
  if (index < 0) {
    dir.files.push({ name: path, ix });
    return;
  }
  const head = path.slice(0, index);
  const rest = path.slice(index + 1);
  let child = dir.dirs.find((candidate) => candidate.name === head);
  if (!child) {
    child = { name: head, dirs: [], files: [] };
    dir.dirs.push(child);
  }
  insert(child, rest, ix);
}

function join(prefix: string, name: string): string {
  return prefix === "" ? name : `${prefix}/${name}`;
}

function toNodes(dir: Dir, prefix: string): DiffNode[] {
  const nodes = dir.dirs.map((child) => dirNode(child, prefix));
  for (const file of dir.files) {
    nodes.push({
      name: file.name,
      key: join(prefix, file.name),
      children: [],
      fileIx: file.ix,
    });
  }
  return nodes;
}

/** 目录节点：向下吞并唯一子目录直到分支点，形成一个合并节点。 */
function dirNode(dir: Dir, prefix: string): DiffNode {
  let name = dir.name;
  let current = dir;
  while (current.files.length === 0 && current.dirs.length === 1) {
    current = current.dirs[0];
    name = `${name}/${current.name}`;
  }
  const key = join(prefix, name);
  return { name, key, children: toNodes(current, key), fileIx: null };
}

/** 由改动文件列表构建树：目录在前、文件在后，各自保持原有顺序。 */
export function buildDiffTree(files: readonly GitDiffFile[]): DiffNode[] {
  const root: Dir = { name: "", dirs: [], files: [] };
  files.forEach((file, ix) => insert(root, file.path, ix));
  return toNodes(root, "");
}

/** 深度优先展开所有节点键（折叠状态遍历用）。 */
export function allKeys(nodes: readonly DiffNode[]): string[] {
  const keys: string[] = [];
  for (const node of nodes) {
    keys.push(node.key);
    keys.push(...allKeys(node.children));
  }
  return keys;
}
