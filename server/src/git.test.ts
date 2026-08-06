import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { GitRunner } from "./git.js";
import { tmpDir } from "./testutil.js";

function git(cwd: string, ...args: string[]): string {
  return execFileSync("git", ["-C", cwd, ...args], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
}

function initRepo(): string {
  const dir = tmpDir("amux-git-");
  git(dir, "init", "-b", "main", "-q");
  git(dir, "config", "user.email", "t@t");
  git(dir, "config", "user.name", "t");
  writeFileSync(join(dir, "a.txt"), "line1\nline2\nline3\n");
  writeFileSync(join(dir, "b.txt"), "b1\nb2\n");
  git(dir, "add", ".");
  git(dir, "commit", "-m", "init", "-q");
  return dir;
}

/** 从 diff 文本中取第 n 个 hunk 的完整补丁（含头部），用于 hunk 级撤销。 */
function hunkPatch(diff: string, n: number): string {
  const lines = diff.split("\n");
  const hunkIdx: number[] = [];
  lines.forEach((l, i) => {
    if (l.startsWith("@@")) hunkIdx.push(i);
  });
  expect(hunkIdx.length).toBeGreaterThan(n);
  const start = hunkIdx[n];
  const end = n + 1 < hunkIdx.length ? hunkIdx[n + 1] : lines.length;
  return lines.slice(0, start).concat(lines.slice(start, end)).join("\n") + "\n";
}

describe("GitRunner（真实 git 仓库）", () => {
  it("status：显示分支、修改/未跟踪文件与行数统计", async () => {
    const dir = initRepo();
    writeFileSync(join(dir, "a.txt"), "line1\nCHANGED\nline3\n");
    writeFileSync(join(dir, "new.txt"), "new\n");
    const runner = new GitRunner();
    const st = await runner.status(dir);
    expect(st.branch).toBe("main");
    const a = st.changes.find((c) => c.path === "a.txt");
    expect(a).toMatchObject({ status: "modified", additions: 1, deletions: 1, staged: false });
    const n = st.changes.find((c) => c.path === "new.txt");
    expect(n).toMatchObject({ status: "untracked", additions: 0, deletions: 0 });
  });

  it("diff：包含修改文件的补丁；指定 path 只返回该文件", async () => {
    const dir = initRepo();
    writeFileSync(join(dir, "a.txt"), "line1\nCHANGED\nline3\n");
    const runner = new GitRunner();
    const all = await runner.diff(dir);
    expect(all).toContain("diff --git a/a.txt b/a.txt");
    expect(all).toContain("-line2");
    expect(all).toContain("+CHANGED");
    const one = await runner.diff(dir, "a.txt");
    expect(one).toContain("a.txt");
    expect(one).not.toContain("b.txt");
  });

  it("push：推送到本地 bare 仓库；无 remote 时返回 ok:false", async () => {
    const dir = initRepo();
    const bare = tmpDir("amux-remote-");
    git(bare, "init", "--bare", "-q");
    git(dir, "remote", "add", "origin", bare);
    writeFileSync(join(dir, "a.txt"), "line1\nline2\nline3\nPUSHED\n");
    git(dir, "add", ".");
    git(dir, "commit", "-m", "second", "-q");
    const runner = new GitRunner();
    const res = await runner.push(dir);
    expect(res.ok).toBe(true);
    // bare 仓库收到该提交
    const rev = git(bare, "rev-parse", "main").trim();
    expect(rev.length).toBe(40);

    const dir2 = initRepo();
    const res2 = await runner.push(dir2);
    expect(res2.ok).toBe(false);
    expect(res2.message).toBeTruthy();
  });

  it("revert 文件：恢复被修改的文件（index + worktree）", async () => {
    const dir = initRepo();
    writeFileSync(join(dir, "a.txt"), "line1\nCHANGED\nline3\n");
    git(dir, "add", "a.txt"); // 同时有 staged 与 worktree 变更
    writeFileSync(join(dir, "a.txt"), "line1\nCHANGED2\nline3\n");
    const runner = new GitRunner();
    const res = await runner.revert(dir, { path: "a.txt" });
    expect(res.ok).toBe(true);
    expect(readFileSync(join(dir, "a.txt"), "utf8")).toBe("line1\nline2\nline3\n");
    const st = await runner.status(dir);
    expect(st.changes.find((c) => c.path === "a.txt")).toBeUndefined();
  });

  it("revert 全部：撤销所有已跟踪变更", async () => {
    const dir = initRepo();
    writeFileSync(join(dir, "a.txt"), "CHANGED\n");
    writeFileSync(join(dir, "b.txt"), "b1\nB2\n");
    const runner = new GitRunner();
    const res = await runner.revert(dir, {});
    expect(res.ok).toBe(true);
    const st = await runner.status(dir);
    expect(st.changes.filter((c) => c.status !== "untracked")).toEqual([]);
  });

  it("revert hunk：只撤销指定 hunk", async () => {
    const dir = initRepo();
    // 16 行文件，改第 2 行与第 14 行 → 两个相隔足够远的 hunk
    const lines = Array.from({ length: 16 }, (_, i) => `l${i + 1}`);
    writeFileSync(join(dir, "a.txt"), lines.join("\n") + "\n");
    git(dir, "add", "a.txt");
    git(dir, "commit", "-m", "sixteen", "-q");
    const modified = [...lines];
    modified[1] = "A2";
    modified[13] = "B14";
    writeFileSync(join(dir, "a.txt"), modified.join("\n") + "\n");
    const runner = new GitRunner();
    const diff = await runner.diff(dir, "a.txt");
    expect(diff.match(/^@@/gm)?.length).toBe(2);
    const firstHunk = hunkPatch(diff, 0);
    const res = await runner.revert(dir, { path: "a.txt", patch: firstHunk });
    expect(res.ok).toBe(true);
    const content = readFileSync(join(dir, "a.txt"), "utf8");
    expect(content).toContain("B14"); // 第二个 hunk 保留
    expect(content).not.toContain("A2"); // 第一个 hunk 已撤销
    expect(content).toContain("l2");
  });
});
