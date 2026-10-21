import { describe, expect, it } from "vitest";

import {
  branchLabel,
  preferredDiffBase,
  preferredRemoteDefault,
} from "./branch";
import type { GitBranch } from "./types";

function local(name: string): GitBranch {
  return {
    name,
    fullName: `refs/heads/${name}`,
    kind: "local",
    isWorktreeBase: false,
    isRemoteDefault: false,
  };
}

function remote(remote: string, branch: string, isRemoteDefault = false): GitBranch {
  return {
    name: `${remote}/${branch}`,
    fullName: `refs/remotes/${remote}/${branch}`,
    kind: "remote",
    remote,
    remoteBranch: branch,
    isWorktreeBase: false,
    isRemoteDefault,
  };
}

describe("分支选择", () => {
  it("远程默认分支优先 origin，并忽略本地分支", () => {
    const upstream = remote("upstream", "main", true);
    const origin = remote("origin", "main", true);
    const branches = [local("main"), upstream, origin];

    expect(preferredRemoteDefault(branches)).toBe(origin);
    expect(preferredRemoteDefault([local("main"), upstream])).toBe(upstream);
    expect(preferredRemoteDefault([local("main")])).toBeUndefined();
  });

  it("改动审查使用 worktree 基分支，否则使用远程默认分支", () => {
    const worktreeBase = { ...local("feature"), isWorktreeBase: true };
    const origin = remote("origin", "main", true);

    expect(preferredDiffBase([worktreeBase, origin], true)).toBe("refs/heads/feature");
    expect(preferredDiffBase([worktreeBase, origin], false)).toBe("refs/remotes/origin/main");
    expect(preferredDiffBase([local("feature")], false)).toBeNull();
  });

  it("本地和远程跟踪分支使用不同前缀", () => {
    expect(branchLabel(local("main"))).toBe("本地 main");
    expect(branchLabel(remote("origin", "main"))).toBe("远程 origin/main");
  });
});
