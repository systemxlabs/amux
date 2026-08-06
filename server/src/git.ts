/**
 * git 直连运行器（git CLI 子进程）。
 * 只读：status / diff；无需判断的写操作：push / revert（undo）。
 * commit / submit PR 等需要编写内容的操作不在本层（由 GUI 拼 prompt 让 agent 执行）。
 */

import { execFile } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import type { GitChange, GitStatusResult } from "shared";

const execFileP = promisify(execFile);

export class GitError extends Error {
  constructor(
    message: string,
    public readonly stdout = "",
    public readonly stderr = "",
  ) {
    super(message);
    this.name = "GitError";
  }
}

interface RunResult {
  stdout: string;
  stderr: string;
}

export class GitRunner {
  private async run(cwd: string, args: string[]): Promise<RunResult> {
    try {
      return await execFileP("git", ["-C", cwd, ...args], { maxBuffer: 64 * 1024 * 1024 });
    } catch (e) {
      const err = e as { stdout?: string; stderr?: string; message: string };
      throw new GitError(err.message, err.stdout ?? "", err.stderr ?? "");
    }
  }

  async status(cwd: string): Promise<GitStatusResult> {
    const branch = await this.branch(cwd);
    const { stdout: porcelain } = await this.run(cwd, ["status", "--porcelain=v1"]);
    const numstat = await this.numstat(cwd);

    const changes: GitChange[] = [];
    for (const line of porcelain.split("\n")) {
      if (!line) continue;
      const xy = line.slice(0, 2);
      const rest = line.slice(3);
      if (xy === "??") {
        changes.push({ path: rest, status: "untracked", staged: false, additions: 0, deletions: 0 });
        continue;
      }
      const X = xy[0];
      const Y = xy[1];
      const path = rest.split(" -> ")[1] ?? rest; // 重命名取新路径
      let status: GitChange["status"];
      if (X === "A" || Y === "A") status = "added";
      else if (X === "D" || Y === "D") status = "deleted";
      else if (X === "R" || Y === "R") status = "renamed";
      else status = "modified";
      const staged = X !== " " && X !== "?";
      const [additions, deletions] = numstat.get(path) ?? [0, 0];
      changes.push({ path, status, staged, additions, deletions });
    }
    return { branch, changes };
  }

  private async branch(cwd: string): Promise<string> {
    const { stdout } = await this.run(cwd, ["rev-parse", "--abbrev-ref", "HEAD"]);
    return stdout.trim() || "HEAD";
  }

  /** 每文件相对 HEAD 的增删行数；无 HEAD（空仓）时返回空表。 */
  private async numstat(cwd: string): Promise<Map<string, [number, number]>> {
    const map = new Map<string, [number, number]>();
    let stdout: string;
    try {
      ({ stdout } = await this.run(cwd, ["diff", "HEAD", "--numstat"]));
    } catch {
      return map; // unborn HEAD
    }
    for (const line of stdout.split("\n")) {
      if (!line) continue;
      const parts = line.split("\t");
      if (parts.length < 3) continue;
      const adds = Number(parts[0]) || 0;
      const dels = Number(parts[1]) || 0;
      const path = parts.slice(2).join("\t");
      map.set(path, [adds, dels]);
    }
    return map;
  }

  async diff(cwd: string, path?: string): Promise<string> {
    const args = ["diff", "HEAD"];
    if (path) args.push("--", path);
    try {
      const { stdout } = await this.run(cwd, args);
      return stdout;
    } catch {
      return "";
    }
  }

  async push(cwd: string): Promise<{ ok: boolean; message?: string }> {
    try {
      // origin HEAD：推送当前分支同名远端分支，无需先设 upstream
      await this.run(cwd, ["push", "origin", "HEAD"]);
      return { ok: true };
    } catch (e) {
      const err = e as GitError;
      const message = err.stderr.trim() || err.stdout.trim() || err.message;
      return { ok: false, message };
    }
  }

  /**
   * 撤销工作区变更（undo 语义；调用方须保证会话空闲）。
   * - 无 path 无 patch：撤销全部已跟踪变更（index + worktree）
   * - 仅 path：撤销该文件变更
   * - path + patch：对 path 反向应用 patch（hunk 级撤销）
   */
  async revert(cwd: string, opts: { path?: string; patch?: string }): Promise<{ ok: boolean; message?: string }> {
    if (opts.patch !== undefined) {
      const dir = mkdtempSync(join(tmpdir(), "amux-revert-"));
      const patchFile = join(dir, "revert.patch");
      try {
        writeFileSync(patchFile, opts.patch);
        await this.run(cwd, ["apply", "--reverse", patchFile]);
        return { ok: true };
      } catch (e) {
        const err = e as GitError;
        return { ok: false, message: err.stderr.trim() || err.message };
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    }
    const target = opts.path ?? ".";
    try {
      await this.run(cwd, ["restore", "--staged", "--worktree", "--", target]);
      return { ok: true };
    } catch (e) {
      const err = e as GitError;
      return { ok: false, message: err.stderr.trim() || err.message };
    }
  }
}
