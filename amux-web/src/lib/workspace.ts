// 路径与工作目录联想（docs/DESIGN.md「新建会话视图」、docs/PRD.md「工作目录视图」）。

import type { FsEntry } from "./types";

/**
 * 工作目录输入拆分为（要列的目录, 名前缀）：
 * `/home/tom/` → 列 `/home/tom/` 下全部条目；`/home/tom` → 列 `/home/` 下与 `tom` 前缀匹配的条目。
 *
 * 输入不含路径分隔符（相对路径或空串）时返回 null，不做联想。
 */
export function splitDirQuery(text: string): { dir: string; prefix: string } | null {
  const index = text.lastIndexOf("/");
  if (index < 0) return null;
  return { dir: text.slice(0, index + 1), prefix: text.slice(index + 1) };
}

/** 目录联想候选：名称以输入前缀开头（前缀为空时全部）。 */
export function matchingPrefix(entries: readonly FsEntry[], prefix: string): FsEntry[] {
  return prefix === "" ? [...entries] : entries.filter((entry) => entry.name.startsWith(prefix));
}

/** 仅目录项（联想列表只列目录）。 */
export function dirsOnly(entries: readonly FsEntry[]): FsEntry[] {
  return entries.filter((entry) => entry.isDir);
}

/** 目录项排序：目录在前，各自按名称升序。 */
export function sortEntriesByName(entries: readonly FsEntry[]): FsEntry[] {
  return [...entries].sort((a, b) => {
    if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
}

/**
 * `path` 是否在 `dir` 子树内（含 `dir` 自身）。
 *
 * 按目录边界判断：`/a/bc` 不在 `/a/b` 子树内。
 */
export function isInSubtree(path: string, dir: string): boolean {
  if (path === dir) return true;
  return path.startsWith(dir.endsWith("/") ? dir : `${dir}/`);
}

/** 父目录：`/a/b` → `/a`，`/a` → `/`，无分隔符时返回空串。 */
export function parentDir(path: string): string {
  const index = path.replace(/\/+$/, "").lastIndexOf("/");
  if (index < 0) return "";
  return index === 0 ? "/" : path.slice(0, index);
}

/** 最后一段名称。 */
export function baseName(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const index = trimmed.lastIndexOf("/");
  return index < 0 ? trimmed : trimmed.slice(index + 1);
}

/** 拼接路径（处理重复分隔符）。 */
export function joinPath(dir: string, name: string): string {
  if (dir === "" || dir.endsWith("/")) return `${dir}${name}`;
  return `${dir}/${name}`;
}

/** 从路径逐级展开的目录前缀（用于展示路径链）。 */
export function pathSegments(path: string): string[] {
  return path.split("/").filter((segment) => segment !== "");
}
