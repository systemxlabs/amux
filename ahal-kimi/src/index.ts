/** ahal-kimi：基于 `kimi acp`（Agent Client Protocol over stdio）的驱动 */
export { KimiDriver, createKimiDriver } from "./driver.js";
export { KimiNormalizer } from "./normalize.js";
export type { KimiUpdate } from "./normalize.js";
export type { JsonRpcClient, JsonRpcNotification } from "./jsonrpc.js";
