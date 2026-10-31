// 远程画面上的键鼠采集 → 归一化坐标/扫描码 → invoke 上行
//
// 可靠性设计（P2 修复）：
// - pointerdown 时 setPointerCapture：拖出窗口后 pointerup 仍能送达
// - window 级 pointerup：捕获失效时的兜底释放
// - 窗口失焦 / 页面隐藏：全量释放已按下的键与按钮，避免远端卡键
//   （例如按住 Ctrl+Alt-Tab 切窗口，keyup 永远不会到达本元素）
//
// 上行批处理（P3 修复）：
// - 事件先进批队列，一次 IPC 送达整批（顺序即数组顺序）。逐事件 invoke
//   时，拥塞下旧鼠标坐标会占着 IPC 排队，导致松键事件被拖后——远端
//   看到的是"按住的键迟迟不松"。
// - 批内**相邻**的连续移动只保留最新坐标（中间夹了其他事件则不能合，
//   顺序必须保留）。移动本就按 rAF 节流，合并不损失可达精度。
// - 冲刷窗口约 1ms（setTimeout 0）：同窗口内的事件才合并，不为批量
//   牺牲输入延迟。

import { onUnmounted, ref } from 'vue';
import { isTauri } from '@tauri-apps/api/core';
import { api, type InputEventArg, type ScreenInputTarget } from '../services/api';
import { lookupCode, type ScanCode } from '../services/keymap';

export function useInput(
  canvas: () => HTMLCanvasElement | undefined,
  sessionId: number,
  getTarget?: () => ScreenInputTarget | undefined,
) {
  const cleanups: Array<() => void> = [];
  const unsupportedKey = ref('');
  const captureError = ref('');
  const captureEnabled = ref(false);

  // 当前按住状态（失焦时据此释放）
  const pressedKeys = new Map<string, ScanCode>();
  const pressedButtons = new Set<number>(); // 协议按钮号 1..5
  let captureRequested = false;
  let captureQueue = Promise.resolve();
  let captureVersion = 0;
  let captureArea = '';

  function setKeyboardCapture(
    enabled: boolean,
    area?: { left: number; top: number; right: number; bottom: number },
  ) {
    if (!isTauri()) return;
    const areaKey = enabled ? JSON.stringify(area) : '';
    if (captureRequested === enabled && captureArea === areaKey) return;
    const version = ++captureVersion;
    // Serialize focus transitions so stale enables cannot survive blur/unmount
    // or take over a replacement session. This flag only controls capture
    // requests: keys swallowed by the native hook never reach the DOM, while
    // keys it passes through must always keep their normal forwarding path.
    captureRequested = enabled;
    captureArea = areaKey;
    if (enabled) flush();
    captureQueue = captureQueue
      .then(async () => {
        await sendQueue;
        const target = getTarget?.();
        const captured = target
          ? await api.clientSetKeyboardCapture(sessionId, enabled, area, target)
          : await api.clientSetKeyboardCapture(sessionId, enabled, area);
        if (version === captureVersion) {
          captureRequested = captured;
          captureEnabled.value = captured;
          captureError.value =
            enabled && !captured ? '系统键捕获未启用，Win / Alt+Tab 可能由本机处理' : '';
        }
      })
      .catch((error: unknown) => {
        if (version === captureVersion) {
          captureRequested = false;
          captureEnabled.value = false;
          if (enabled) captureError.value = `系统键捕获失败：${String(error)}`;
        }
      });
  }

  /** 浏览器 button → 协议按钮号（0左 1中 2右 3X1 4X2 → 1..5） */
  function mapButton(b: number): number | null {
    switch (b) {
      case 0:
        return 1;
      case 1:
        return 3;
      case 2:
        return 2;
      case 3:
        return 4;
      case 4:
        return 5;
      default:
        return null;
    }
  }

  // ---- 上行批处理 ----
  let sendQueue = Promise.resolve();
  let pending = 0;
  let failed = false;
  let batch: InputEventArg[] = [];
  let batchTarget: ScreenInputTarget | undefined;
  let flushTimer: ReturnType<typeof setTimeout> | null = null;
  /** 批内上限：超过就立即冲刷，避免无限堆积 */
  const MAX_BATCH = 64;

  function fail() {
    if (failed) return;
    failed = true;
    // 可靠输入失败后无法保证松键送达，结束对应会话让服务端释放按住状态。
    api.clientDisconnect(sessionId).catch(() => undefined);
  }

  function flush() {
    if (flushTimer !== null) {
      clearTimeout(flushTimer);
      flushTimer = null;
    }
    if (batch.length === 0) return;
    const events = batch;
    const target = batchTarget;
    batch = [];
    batchTarget = undefined;
    if (failed) return;
    if (pending >= 128) {
      fail();
      return;
    }
    pending++;
    sendQueue = sendQueue
      .then(async () => {
        if (!failed) {
          if (target) await api.clientSendInputBatch(events, sessionId, target);
          else await api.clientSendInputBatch(events, sessionId);
        }
      })
      .catch(fail)
      .finally(() => {
        pending--;
      });
  }

  function scheduleFlush() {
    if (flushTimer !== null) return;
    flushTimer = setTimeout(flush, 0);
  }

  /** 入队一个事件（批量发送） */
  function send(arg: InputEventArg) {
    if (failed) return;
    const target = getTarget?.();
    if (batch.length && JSON.stringify(target) !== JSON.stringify(batchTarget)) flush();
    batchTarget = target;
    // 入队时合并相邻移动，保留键鼠事件的边界；冲刷时直接转移数组所有权，
    // 避免每批额外创建数组并再次遍历全部事件。
    if (arg.type === 'mouseMove' && batch.at(-1)?.type === 'mouseMove') {
      batch[batch.length - 1] = arg;
    } else {
      batch.push(arg);
    }
    if (batch.length >= MAX_BATCH) {
      flush();
    } else {
      scheduleFlush();
    }
  }

  function sendUp(btn: number) {
    send({ type: 'mouseButton', button: btn, down: false });
  }

  function sendKeyUp(sc: ScanCode) {
    send({ type: 'key', scancode: sc.scancode, extended: sc.extended, down: false });
  }

  /** 释放所有按住的鼠标按键 */
  function releaseAllButtons() {
    for (const btn of [...pressedButtons]) {
      pressedButtons.delete(btn);
      sendUp(btn);
    }
  }

  /** 释放所有按住状态（失焦/隐藏/断开时调用） */
  function releaseAll() {
    releaseAllButtons();
    for (const sc of pressedKeys.values()) {
      sendKeyUp(sc);
    }
    pressedKeys.clear();
  }

  function bind(el: HTMLElement) {
    const enableKeyboardCapture = () => {
      const rect = el.getBoundingClientRect();
      const scale = window.devicePixelRatio || 1;
      setKeyboardCapture(true, {
        left: Math.round(rect.left * scale),
        top: Math.round(rect.top * scale),
        right: Math.round((rect.left + rect.width) * scale),
        bottom: Math.round((rect.top + rect.height) * scale),
      });
    };
    const updateKeyboardCapture = (e: MouseEvent) => {
      const rect = el.getBoundingClientRect();
      const onSurface = e.target === el || e.target === canvas();
      if (
        onSurface &&
        e.clientX >= rect.left &&
        e.clientX < rect.left + rect.width &&
        e.clientY >= rect.top &&
        e.clientY < rect.top + rect.height
      ) {
        enableKeyboardCapture();
      }
    };
    const onPointerLeave = () => {
      // Keep the hook installed for the session. Native cursor/window/rectangle
      // hit testing decides ownership on every key, without an IPC race when
      // the pointer re-enters and a system shortcut is pressed immediately.
      for (const sc of pressedKeys.values()) sendKeyUp(sc);
      pressedKeys.clear();
      flush();
    };
    // ---- 鼠标移动（rAF 节流，只发最新位置）----
    let pendingMove: { x: number; y: number } | null = null;
    let moveScheduled = false;
    let moveRaf = 0;
    const flushMove = () => {
      moveScheduled = false;
      if (pendingMove) {
        send({ type: 'mouseMove', x: pendingMove.x, y: pendingMove.y });
        pendingMove = null;
      }
    };
    /** 同步入队：按下/抬起前把最新坐标排进同一批，保证批内时序
     *  「移动 → 按下」而非「按下 → 移动」（快速移动后点击会点旧位置）。
     *  与按键事件同批发出，一次 IPC 完成，不存在跨批乱序 */
    const flushMoveSync = () => {
      if (moveScheduled) {
        moveScheduled = false;
        cancelAnimationFrame(moveRaf);
      }
      if (pendingMove) {
        send({ type: 'mouseMove', x: pendingMove.x, y: pendingMove.y });
        pendingMove = null;
      }
    };
    const updatePosition = (e: MouseEvent) => {
      const c = canvas();
      if (!c) return;
      const rect = c.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return;
      pendingMove = {
        x: Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width)),
        y: Math.max(0, Math.min(1, (e.clientY - rect.top) / rect.height)),
      };
    };
    const onPointerMove = (e: PointerEvent) => {
      updateKeyboardCapture(e);
      updatePosition(e);
      if (!pendingMove) return;
      if (!moveScheduled) {
        moveScheduled = true;
        moveRaf = requestAnimationFrame(flushMove);
      }
    };

    // ---- 鼠标按键：指针捕获 + 状态跟踪 ----
    const onPointerDown = (e: PointerEvent) => {
      // Pointer events only report the first pressed / last released mouse
      // button. Mouse events below preserve intermediate chord transitions.
      try {
        el.setPointerCapture(e.pointerId);
      } catch {
        /* Pointer capture is unavailable in some environments. */
      }
    };
    const onMouseDown = (e: MouseEvent) => {
      e.preventDefault();
      updatePosition(e);
      flushMoveSync(); // 先送坐标，再送按下
      el.focus();
      updateKeyboardCapture(e);
      const b = mapButton(e.button);
      if (b) {
        pressedButtons.add(b);
        send({ type: 'mouseButton', button: b, down: true });
      }
    };
    const onMouseUp = (e: MouseEvent) => {
      e.preventDefault();
      updatePosition(e);
      flushMoveSync(); // 拖拽结束时坐标也先落地
      const b = mapButton(e.button);
      if (b) {
        if (pressedButtons.delete(b)) sendUp(b);
      } else if (pressedButtons.size > 0) {
        // button 无法映射（异常事件）：按记录状态释放，防止卡键
        releaseAllButtons();
      }
    };
    // pointercancel 的 button 不可信（规范为 -1，映射不出按钮）：
    // 必须按已记录的按键状态释放，不能依赖事件自带的 button
    const onPointerCancel = () => {
      flushMoveSync();
      releaseAllButtons();
      flush();
    };
    const onLostPointerCapture = (e: PointerEvent) => {
      flushMoveSync();
      // Losing capture does not mean that every button was released.
      // Protocol button order matches the bits in MouseEvent.buttons.
      for (const b of pressedButtons) {
        if ((e.buttons & (1 << (b - 1))) === 0) {
          pressedButtons.delete(b);
          sendUp(b);
        }
      }
      flush();
    };
    const onContextMenu = (e: Event) => e.preventDefault();

    // ---- 滚轮 ----
    // 纵向：浏览器 deltaY 正=向下，Windows WHEEL 正=向上 → 取反
    // 横向：浏览器 deltaX 与 Windows HWHEEL 的 mouseData 正方向都是
    // 向右 → 保留符号（曾误一并取反，导致右滑变左滑）
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      updatePosition(e);
      flushMoveSync(); // 滚轮作用于指针所在窗口，不能先滚动再补发鼠标位置。
      const d = e.deltaMode === 0 ? 120 : 1; // pixel → 行换算
      send({
        type: 'wheel',
        dx: e.deltaX / d,
        dy: -(e.deltaY / d),
      });
    };

    // ---- 键盘 ----
    const onKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      const sc = lookupCode(e.code);
      if (!sc) {
        unsupportedKey.value = `暂不支持该按键：${e.code || '未识别按键'}`;
        return;
      }
      unsupportedKey.value = '';
      // 记录按住状态（用于失焦释放）；但**转发所有 keydown**，
      // 包括长按的自动重复——否则远端收不到连续退格/方向键连移
      pressedKeys.set(e.code, sc);
      send({ type: 'key', scancode: sc.scancode, extended: sc.extended, down: true });
    };
    const onKeyUp = (e: KeyboardEvent) => {
      e.preventDefault();
      const sc = lookupCode(e.code);
      if (!sc) return;
      if (pressedKeys.delete(e.code)) {
        sendKeyUp(sc);
      }
    };

    el.addEventListener('pointermove', onPointerMove);
    el.addEventListener('pointerdown', onPointerDown);
    el.addEventListener('mousedown', onMouseDown);
    el.addEventListener('mouseup', onMouseUp);
    el.addEventListener('pointercancel', onPointerCancel);
    el.addEventListener('lostpointercapture', onLostPointerCapture);
    el.addEventListener('contextmenu', onContextMenu);
    el.addEventListener('wheel', onWheel, { passive: false });
    el.addEventListener('keydown', onKeyDown);
    el.addEventListener('keyup', onKeyUp);

    // ---- 窗口级兜底释放（切换窗口或拖出画面时释放按键）----
    const onWindowBlur = () => {
      flushMoveSync();
      releaseAll();
      flush(); // Hidden windows may suspend timers and animation callbacks.
    };
    const onVisibility = () => {
      if (document.visibilityState === 'hidden') {
        setKeyboardCapture(false);
        onWindowBlur();
      } else enableKeyboardCapture();
    };
    const onFocusOut = (e: FocusEvent) => {
      if (!el.contains(e.relatedTarget as Node | null)) onWindowBlur();
    };
    // 元素外的 pointerup（指针捕获未生效时的兜底）
    const onWindowPointerUp = (e: MouseEvent) => {
      if (!el.contains(e.target as Node)) {
        const b = mapButton(e.button);
        if (b && pressedButtons.has(b)) {
          updatePosition(e);
          flushMoveSync();
          pressedButtons.delete(b);
          sendUp(b);
        }
      }
    };
    // 元素外的 keyup（焦点被别的元素拿走时的兜底）
    const onWindowKeyUp = (e: KeyboardEvent) => {
      if (!el.contains(e.target as Node) && pressedKeys.has(e.code)) {
        const sc = pressedKeys.get(e.code)!;
        pressedKeys.delete(e.code);
        sendKeyUp(sc);
      }
    };
    window.addEventListener('blur', onWindowBlur);
    window.addEventListener('pointermove', updateKeyboardCapture, true);
    el.addEventListener('pointerenter', updateKeyboardCapture);
    el.addEventListener('pointerleave', onPointerLeave);
    window.addEventListener('mouseup', onWindowPointerUp);
    window.addEventListener('keyup', onWindowKeyUp);
    document.addEventListener('visibilitychange', onVisibility);
    el.addEventListener('focusout', onFocusOut);

    cleanups.push(() => {
      cancelAnimationFrame(moveRaf);
      pendingMove = null;
      el.removeEventListener('pointermove', onPointerMove);
      el.removeEventListener('pointerdown', onPointerDown);
      el.removeEventListener('mousedown', onMouseDown);
      el.removeEventListener('mouseup', onMouseUp);
      el.removeEventListener('pointercancel', onPointerCancel);
      el.removeEventListener('lostpointercapture', onLostPointerCapture);
      el.removeEventListener('focusout', onFocusOut);
      el.removeEventListener('pointerenter', updateKeyboardCapture);
      el.removeEventListener('pointerleave', onPointerLeave);
      el.removeEventListener('contextmenu', onContextMenu);
      el.removeEventListener('wheel', onWheel);
      el.removeEventListener('keydown', onKeyDown);
      el.removeEventListener('keyup', onKeyUp);
      window.removeEventListener('blur', onWindowBlur);
      window.removeEventListener('pointermove', updateKeyboardCapture, true);
      window.removeEventListener('mouseup', onWindowPointerUp);
      window.removeEventListener('keyup', onWindowKeyUp);
      document.removeEventListener('visibilitychange', onVisibility);
    });
    const resizeObserver =
      typeof ResizeObserver === 'undefined'
        ? null
        : new ResizeObserver(() => {
            if (captureRequested) enableKeyboardCapture();
          });
    resizeObserver?.observe(el);
    cleanups.push(() => resizeObserver?.disconnect());
    enableKeyboardCapture();
  }

  function stop() {
    setKeyboardCapture(false);
    cleanups.splice(0).forEach((fn) => fn());
    releaseAll();
    flush(); // 释放事件与残留输入一起提交，不依赖卸载后的定时器。
  }
  onUnmounted(stop);
  return { bind, releaseAll, stop, unsupportedKey, captureError, captureEnabled };
}
