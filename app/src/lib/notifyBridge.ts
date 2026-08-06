/**
 * 系统通知桥：桌面通知（Web Notification API）+ 点击跳转到对应会话。
 * 通知由客户端从事件流推导（docs/DESIGN.md §4），配置存客户端本地。
 */

let openHandler: ((machineId: string, sessionId: string) => void) | null = null;

export function setNotificationOpenHandler(fn: (machineId: string, sessionId: string) => void): void {
  openHandler = fn;
}

function show(title: string, body: string, machineId?: string, sessionId?: string): void {
  if (typeof Notification === "undefined") return;
  if (Notification.permission === "default") {
    void Notification.requestPermission().then((p) => {
      if (p === "granted") show(title, body, machineId, sessionId);
    });
    return;
  }
  if (Notification.permission !== "granted") return;
  try {
    const n = new Notification(title, { body });
    n.onclick = () => {
      window.focus();
      if (machineId && sessionId) openHandler?.(machineId, sessionId);
    };
  } catch {
    // 通知失败不影响主流程
  }
}

export function notifyWorkEnded(machineId: string, sessionId: string, machineName: string, sessionLabel: string, reason?: string): void {
  show(`工作结束 · ${machineName}`, `${sessionLabel}${reason ? `（${reason}）` : ""}`, machineId, sessionId);
}

export function notifyError(machineId: string, sessionId: string, machineName: string, sessionLabel: string, message: string): void {
  show(`异常 · ${machineName}`, `${sessionLabel}: ${message}`, machineId, sessionId);
}

export function notifyLongIdle(machineId: string, sessionId: string, machineName: string, sessionLabel: string): void {
  show(`长时间无响应 · ${machineName}`, sessionLabel, machineId, sessionId);
}
