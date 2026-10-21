import type { GitBranch } from "./types";

/** 分支选择器中的类型前缀，区分同名本地分支与远程跟踪分支。 */
export function branchDisplayName(branch: GitBranch): string {
  return branch.kind === "local"
    ? `本地 ${branch.name}`
    : `远程 ${branch.remote}/${branch.remoteBranch}`;
}

export function branchLabel(branch: GitBranch): string {
  return branchDisplayName(branch);
}

/** 多个远程都有默认分支时优先 origin，否则取第一个。 */
export function preferredRemoteDefault(branches: readonly GitBranch[]): GitBranch | undefined {
  const defaults = branches.filter(
    (branch) => branch.kind === "remote" && branch.isRemoteDefault,
  );
  return (
    defaults.find((branch) => branch.kind === "remote" && branch.remote === "origin") ??
    defaults[0]
  );
}

/** 改动审查的基准分支；无法识别默认分支时返回 null，由用户手动选择。 */
export function preferredDiffBase(
  branches: readonly GitBranch[],
  useWorktree: boolean,
): string | null {
  if (useWorktree) {
    const worktreeBase = branches.find((branch) => branch.isWorktreeBase);
    if (worktreeBase !== undefined) return worktreeBase.fullName;
  }
  return preferredRemoteDefault(branches)?.fullName ?? null;
}
