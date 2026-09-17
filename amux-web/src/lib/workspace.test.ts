// 验证工作目录联想的两个文档样例与路径工具。

import { describe, expect, it } from "vitest";

import type { FsEntry } from "./types";
import {
  baseName,
  dirsOnly,
  isInSubtree,
  joinPath,
  matchingPrefix,
  parentDir,
  pathSegments,
  sortEntriesByName,
  splitDirQuery,
} from "./workspace";

function dir(name: string, path: string): FsEntry {
  return { name, path, isDir: true, size: 0 };
}

describe("splitDirQuery", () => {
  it("以分隔符结尾时列出该目录下全部条目（DESIGN 样例）", () => {
    expect(splitDirQuery("/home/tom/")).toEqual({ dir: "/home/tom/", prefix: "" });
  });

  it("未以分隔符结尾时列出上一级目录并按最后一段做前缀匹配（DESIGN 样例）", () => {
    expect(splitDirQuery("/home/tom")).toEqual({ dir: "/home/", prefix: "tom" });
  });

  it("不含分隔符的输入不做联想", () => {
    expect(splitDirQuery("")).toBeNull();
    expect(splitDirQuery("relative")).toBeNull();
  });
});

describe("matchingPrefix", () => {
  it("前缀为空时保留全部条目", () => {
    const entries = [dir("tom", "/home/tom"), dir("jerry", "/home/jerry")];
    expect(matchingPrefix(entries, "")).toHaveLength(2);
  });

  it("按名称前缀过滤（/home/tom → /home/ 下 tom 开头项）", () => {
    const entries = [
      dir("tom", "/home/tom"),
      dir("tomcat", "/home/tomcat"),
      dir("jerry", "/home/jerry"),
    ];
    expect(matchingPrefix(entries, "tom").map((entry) => entry.name)).toEqual(["tom", "tomcat"]);
  });
});

describe("路径工具", () => {
  it("只保留目录项", () => {
    const entries = [dir("src", "/w/src"), { name: "a.rs", path: "/w/a.rs", isDir: false, size: 1 }];
    expect(dirsOnly(entries).map((entry) => entry.name)).toEqual(["src"]);
  });

  it("目录在前、名称升序", () => {
    const entries = [
      { name: "b.rs", path: "/w/b.rs", isDir: false, size: 1 },
      dir("z", "/w/z"),
      dir("a", "/w/a"),
    ];
    expect(sortEntriesByName(entries).map((entry) => entry.name)).toEqual(["a", "z", "b.rs"]);
  });

  it("取父目录与末段名称", () => {
    expect(parentDir("/a/b")).toBe("/a");
    expect(parentDir("/a")).toBe("/");
    expect(parentDir("a")).toBe("");
    expect(baseName("/a/b/")).toBe("b");
    expect(baseName("b")).toBe("b");
  });

  it("拼接路径不产生重复分隔符", () => {
    expect(joinPath("/a/", "b")).toBe("/a/b");
    expect(joinPath("/a", "b")).toBe("/a/b");
    expect(joinPath("", "b")).toBe("b");
  });

  it("拆出路径段", () => {
    expect(pathSegments("/a/b/c")).toEqual(["a", "b", "c"]);
  });

  it("子树判断按目录边界：同名前缀的兄弟目录不算在内", () => {
    expect(isInSubtree("/a/b", "/a")).toBe(true);
    expect(isInSubtree("/a/b/c", "/a")).toBe(true);
    expect(isInSubtree("/a", "/a")).toBe(true);
    expect(isInSubtree("/a/bc", "/a/b")).toBe(false);
    expect(isInSubtree("/a", "/a/b")).toBe(false);
    expect(isInSubtree("/a/b", "/")).toBe(true);
  });
});
