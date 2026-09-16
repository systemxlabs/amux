// localStorage token 读写与清除（DESIGN「Web 应用 - 连接存储」）。

import { describe, expect, it } from "vitest";

import { clearToken, loadToken, saveToken, TOKEN_KEY, type StorageLike } from "./token";

/** 最小内存存储：键值行为与 localStorage 一致。 */
function memoryStorage(): StorageLike & { keys(): string[] } {
  const map = new Map<string, string>();
  return {
    getItem: (key) => map.get(key) ?? null,
    setItem: (key, value) => void map.set(key, value),
    removeItem: (key) => void map.delete(key),
    keys: () => [...map.keys()],
  };
}

describe("token 存储", () => {
  it("初始无 token 时读出空串（登录页据此不展示连接错误）", () => {
    expect(loadToken(memoryStorage())).toBe("");
  });

  it("保存后按固定键写入并可读回", () => {
    const storage = memoryStorage();
    saveToken("tk-1", storage);
    expect(loadToken(storage)).toBe("tk-1");
    expect(storage.keys()).toEqual([TOKEN_KEY]);
  });

  it("清除后读出空串", () => {
    const storage = memoryStorage();
    saveToken("tk-1", storage);
    clearToken(storage);
    expect(loadToken(storage)).toBe("");
    expect(storage.keys()).toEqual([]);
  });

  it("存储不可用时读写不抛错", () => {
    expect(loadToken(null)).toBe("");
    expect(() => saveToken("tk", null)).not.toThrow();
    expect(() => clearToken(null)).not.toThrow();
  });
});
