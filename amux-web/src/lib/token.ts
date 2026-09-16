// 连接 token 的本地存储（docs/DESIGN.md「Web 应用 - 连接存储」：token 存浏览器 localStorage）。
//
// Web 端固定同源访问 Server，本地只保存 token。

/** localStorage 键。 */
export const TOKEN_KEY = "amux.token";

/** 只用到的最小存储接口（便于在没有 DOM 的环境里用内存实现驱动）。 */
export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** 浏览器 localStorage；不可用时（隐私模式等）返回 null。 */
export function browserStorage(): StorageLike | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

/** 读取本地 token；无 token 返回空串（登录页据此判断是否展示连接错误）。 */
export function loadToken(storage: StorageLike | null = browserStorage()): string {
  return storage?.getItem(TOKEN_KEY) ?? "";
}

export function saveToken(token: string, storage: StorageLike | null = browserStorage()): void {
  storage?.setItem(TOKEN_KEY, token);
}

export function clearToken(storage: StorageLike | null = browserStorage()): void {
  storage?.removeItem(TOKEN_KEY);
}
