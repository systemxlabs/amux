// 改动文件树（PRD「改动审查视图」的示例树与合并规则）。

import { describe, expect, it } from "vitest";

import { allKeys, buildDiffTree, type DiffNode } from "./difftree";
import type { GitDiffFile } from "./types";

function file(path: string): GitDiffFile {
  return { path, status: "modified", additions: 1, deletions: 1, hunks: [] };
}

function keys(nodes: readonly DiffNode[]): string[] {
  return allKeys(nodes);
}

describe("buildDiffTree", () => {
  it("合并不含改动文件的中间目录（PRD 示例树）", () => {
    const tree = buildDiffTree([
      file("src/catalog/helper/query.rs"),
      file("src/catalog/schema.rs"),
      file("src/storage/s3/parquet.rs"),
    ]);
    expect(keys(tree)).toEqual([
      "src",
      "src/catalog",
      "src/catalog/helper",
      "src/catalog/helper/query.rs",
      "src/catalog/schema.rs",
      "src/storage/s3",
      "src/storage/s3/parquet.rs",
    ]);
    const src = tree[0];
    expect(src.children.map((node) => node.name)).toEqual(["catalog", "storage/s3"]);
    // 分支点不再合并：catalog 下有目录 helper 与文件 schema.rs
    const catalog = src.children[0];
    expect(catalog.children.map((node) => node.name)).toEqual(["helper", "schema.rs"]);
    expect(catalog.children[1].fileIx).toBe(1);
    expect(catalog.children[0].children[0].fileIx).toBe(0);
  });

  it("根目录下文件与目录同级，文件带改动列表下标", () => {
    const tree = buildDiffTree([file("Cargo.toml"), file("src/main.rs")]);
    expect(keys(tree)).toEqual(["src", "src/main.rs", "Cargo.toml"]);
    expect(tree[1].fileIx).toBe(0);
  });

  it("空改动列表得到空树", () => {
    expect(buildDiffTree([])).toEqual([]);
  });
});
