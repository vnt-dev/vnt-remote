// 剪贴板文件传输面板状态。
//
// Rust 侧每个会话独立收发文件，进度经 `clipboard://transfer` 事件推送
// （按本机连接标识过滤）。这里只做展示与取消：
// - 进行中的传输实时更新进度
// - 结束的传输保留几秒后自动移除（避免刚完成就消失看不清）
// - 取消走 invoke，实际清理在 Rust 侧（释放源文件句柄并通知对端）

import { onUnmounted, ref } from 'vue';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { api, type ClipboardTransfer } from '../services/api';
import { loadClipboardSyncPref } from '../services/prefs';

/** 传输结束后在面板中停留的时长。 */
const KEEP_FINISHED_MS = 5000;

/**
 * @param initialEnabled 会话初始同步状态；缺省取主控「高级选项」的偏好。
 *   被控端为观看模式时传 false（Rust 侧注册已禁用，前端状态保持一致）。
 */
export function useClipboard(
  sessionId: number,
  connectionId: string,
  initialEnabled?: boolean,
  shared = false,
) {
  const transfers = ref<ClipboardTransfer[]>([]);
  // 会话初始同步状态取主控「高级选项」的剪贴板同步偏好
  const enabled = ref(initialEnabled ?? loadClipboardSyncPref());
  const unlisteners: UnlistenFn[] = [];
  const timers = new Map<number, ReturnType<typeof setTimeout>>();
  let disposed = false;

  function forget(map: Map<number, ReturnType<typeof setTimeout>>, transferId: number) {
    const timer = map.get(transferId);
    if (timer !== undefined) {
      clearTimeout(timer);
      map.delete(transferId);
    }
  }

  function upsert(list: ClipboardTransfer[], event: ClipboardTransfer): ClipboardTransfer[] {
    const index = list.findIndex((t) => t.transferId === event.transferId);
    if (index >= 0) {
      const next = [...list];
      next[index] = event;
      return next;
    }
    return [...list, event];
  }

  function apply(event: ClipboardTransfer) {
    if (disposed || event.sessionId !== sessionId || event.connectionId !== connectionId) return;
    transfers.value = upsert(transfers.value, event);
    if (event.state === 'active') {
      forget(timers, event.transferId);
    } else {
      // 终态：面板停留一段时间后移除，避免面板残留
      forget(timers, event.transferId);
      timers.set(
        event.transferId,
        setTimeout(() => {
          timers.delete(event.transferId);
          transfers.value = transfers.value.filter((t) => t.transferId !== event.transferId);
        }, KEEP_FINISHED_MS),
      );
    }
  }

  function subscribe(): Promise<void> {
    void listen<{ connectionId: string; enabled: boolean }>('clipboard://enabled', (event) => {
      if (!disposed && event.payload.connectionId === connectionId)
        enabled.value = event.payload.enabled;
    })
      .then((unlisten) => {
        if (disposed) unlisten();
        else unlisteners.push(unlisten);
      })
      .catch(() => undefined);
    return listen<ClipboardTransfer>('clipboard://transfer', (ev) => apply(ev.payload))
      .then(async (unlisten) => {
        if (disposed) unlisten();
        else {
          unlisteners.push(unlisten);
          if (shared) {
            const current = await api.clientClipboardEnabled(connectionId);
            if (!disposed) enabled.value = current;
          }
        }
      })
      .catch(() => undefined);
  }

  async function cancel(transferId: number) {
    try {
      await api.clipboardCancelTransfer(connectionId, transferId);
    } catch {
      // 传输可能已结束：忽略，事件流会纠正面板状态
    }
  }

  async function setEnabled(next: boolean) {
    enabled.value = next;
    try {
      await api.clientSetClipboardEnabled(connectionId, next);
    } catch {
      // 会话可能已断开：保持界面开关状态，不打断用户操作
    }
  }

  onUnmounted(() => {
    disposed = true;
    timers.forEach((timer) => clearTimeout(timer));
    timers.clear();
    unlisteners.forEach((fn) => fn());
  });

  return { transfers, enabled, subscribe, cancel, setEnabled };
}
