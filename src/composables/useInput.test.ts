import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useInput } from './useInput';
import { api } from '../services/api';
import { isTauri } from '@tauri-apps/api/core';

vi.mock('@tauri-apps/api/core', () => ({ isTauri: vi.fn(() => false) }));

vi.mock('vue', async (importOriginal) => ({
  ...(await importOriginal<typeof import('vue')>()),
  onUnmounted: vi.fn(),
}));
vi.mock('../services/api', () => ({
  api: {
    clientSendInputBatch: vi.fn().mockResolvedValue(undefined),
    clientDisconnect: vi.fn().mockResolvedValue(undefined),
    clientSetKeyboardCapture: vi.fn().mockImplementation(async (_, enabled: boolean) => enabled),
  },
}));

// Native EventTarget preserves listener removal and event dispatch behavior without a DOM dependency.
class Stage extends EventTarget {
  getBoundingClientRect = () => ({ left: 100, top: 50, width: 400, height: 200 });
  focus = vi.fn();
  setPointerCapture = vi.fn();
  hasPointerCapture = () => false;
  contains(target: unknown) {
    return target === this;
  }
}
function dispatch(target: EventTarget, type: string, fields: Record<string, unknown> = {}) {
  const event = new Event(type, { cancelable: true });
  Object.assign(event, fields);
  target.dispatchEvent(event);
}
const pointer = (clientX: number, clientY: number) => ({
  clientX,
  clientY,
  button: 0,
  pointerId: 1,
});
const sent = () => vi.mocked(api.clientSendInputBatch).mock.calls.flatMap(([events]) => events);
const captureArea = { left: 100, top: 50, right: 500, bottom: 250 };

describe('remote input', () => {
  let stage: Stage;
  let input: ReturnType<typeof useInput>;
  let rect: { left: number; top: number; width: number; height: number };
  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    vi.mocked(isTauri).mockReturnValue(false);
    vi.stubGlobal('window', new EventTarget());
    vi.stubGlobal('document', new EventTarget());
    vi.stubGlobal('requestAnimationFrame', (cb: () => void) => setTimeout(cb, 16));
    vi.stubGlobal('cancelAnimationFrame', (id: number) => clearTimeout(id));
    stage = new Stage();
    rect = { left: 100, top: 50, width: 400, height: 200 };
    input = useInput(() => ({ getBoundingClientRect: () => rect }) as HTMLCanvasElement, 42);
    input.bind(stage as unknown as HTMLElement);
  });
  afterEach(async () => {
    input.stop();
    await vi.runAllTimersAsync();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('forwards keys passed through by the native hook without releasing mouse chords', async () => {
    vi.mocked(isTauri).mockReturnValue(true);
    dispatch(stage, 'mousedown', pointer(200, 100));
    dispatch(stage, 'keydown', { code: 'AltLeft' });
    dispatch(stage, 'keydown', { code: 'Tab' });
    dispatch(stage, 'keyup', { code: 'Tab' });
    dispatch(stage, 'keyup', { code: 'AltLeft' });
    dispatch(stage, 'mousedown', { ...pointer(200, 100), button: 2 });
    await vi.runAllTimersAsync();
    expect(api.clientSetKeyboardCapture).toHaveBeenCalledExactlyOnceWith(42, true, captureArea);
    expect(sent().filter((e) => e.type === 'key')).toEqual([
      { type: 'key', scancode: 0x38, extended: false, down: true },
      { type: 'key', scancode: 0x0f, extended: false, down: true },
      { type: 'key', scancode: 0x0f, extended: false, down: false },
      { type: 'key', scancode: 0x38, extended: false, down: false },
    ]);
    expect(sent().filter((e) => e.type === 'mouseButton')).toEqual([
      { type: 'mouseButton', button: 1, down: true },
      { type: 'mouseButton', button: 2, down: true },
    ]);
    dispatch(stage, 'pointerleave');
    await vi.runAllTimersAsync();
    expect(api.clientSetKeyboardCapture).toHaveBeenCalledTimes(1);
  });

  it.each(['pending', 'enabled', 'unavailable', 'failed'] as const)(
    'forwards ordinary DOM keys while native capture is %s',
    async (status) => {
      vi.mocked(isTauri).mockReturnValue(true);
      if (status === 'unavailable') {
        vi.mocked(api.clientSetKeyboardCapture).mockResolvedValueOnce(false);
      } else if (status === 'failed') {
        vi.mocked(api.clientSetKeyboardCapture).mockRejectedValueOnce(new Error('hook failed'));
      }
      dispatch(stage, 'mousedown', pointer(200, 100));
      if (status !== 'pending') await vi.runAllTimersAsync();
      // Injected input and input skipped by a non-matching native window still
      // arrive as DOM events and must use the normal reliable input queue.
      for (const code of ['KeyA', 'Backspace', 'Enter', 'ArrowLeft']) {
        dispatch(stage, 'keydown', { code });
        dispatch(stage, 'keyup', { code });
      }
      await vi.runAllTimersAsync();
      expect(sent().filter((e) => e.type === 'key')).toEqual(
        [
          { scancode: 0x1e, extended: false },
          { scancode: 0x0e, extended: false },
          { scancode: 0x1c, extended: false },
          { scancode: 0x4b, extended: true },
        ].flatMap((sc) => [
          { type: 'key', ...sc, down: true },
          { type: 'key', ...sc, down: false },
        ]),
      );
    },
  );

  it('preserves a DOM modifier held across native capture activation until its keyup', async () => {
    vi.mocked(isTauri).mockReturnValue(true);
    dispatch(stage, 'keydown', { code: 'AltLeft' });
    await vi.runAllTimersAsync();
    dispatch(stage, 'mousedown', pointer(200, 100));
    await vi.runAllTimersAsync();
    expect(sent().filter((e) => e.type === 'key')).toEqual([
      { type: 'key', scancode: 0x38, extended: false, down: true },
    ]);
    dispatch(stage, 'keyup', { code: 'AltLeft' });
    await vi.runAllTimersAsync();
    expect(sent().filter((e) => e.type === 'key')).toEqual([
      { type: 'key', scancode: 0x38, extended: false, down: true },
      { type: 'key', scancode: 0x38, extended: false, down: false },
    ]);
  });

  it('serializes a pending capture enable before disabling it on stop', async () => {
    vi.mocked(isTauri).mockReturnValue(true);
    dispatch(stage, 'mousedown', pointer(200, 100));
    input.stop();
    await vi.runAllTimersAsync();
    expect(vi.mocked(api.clientSetKeyboardCapture).mock.calls).toEqual([
      [42, true, captureArea],
      [42, false, undefined],
    ]);
  });

  it('resumes mouse and keyboard forwarding after desktop transitions', async () => {
    for (let generation = 0; generation < 3; generation++) {
      input.stop();
      await vi.runAllTimersAsync();
      vi.mocked(api.clientSendInputBatch).mockClear();
      dispatch(stage, 'mousedown', pointer(200, 100));
      dispatch(stage, 'keydown', { code: 'KeyA' });
      await vi.runAllTimersAsync();
      expect(sent()).toEqual([]);

      input.bind(stage as unknown as HTMLElement);
      dispatch(stage, 'mousedown', pointer(200, 100));
      dispatch(stage, 'mouseup', pointer(200, 100));
      dispatch(stage, 'keydown', { code: 'KeyA' });
      dispatch(stage, 'keyup', { code: 'KeyA' });
      await vi.runAllTimersAsync();
      expect(sent().filter((event) => event.type === 'mouseButton')).toEqual([
        { type: 'mouseButton', button: 1, down: true },
        { type: 'mouseButton', button: 1, down: false },
      ]);
      expect(sent().filter((event) => event.type === 'key')).toEqual([
        { type: 'key', scancode: 0x1e, extended: false, down: true },
        { type: 'key', scancode: 0x1e, extended: false, down: false },
      ]);
    }
  });

  it('keeps the native hook ready while native hit testing excludes the local toolbar', async () => {
    vi.mocked(isTauri).mockReturnValue(true);
    dispatch(stage, 'mousedown', pointer(200, 100));
    dispatch(window, 'pointermove', pointer(200, 60));
    await vi.runAllTimersAsync();
    expect(api.clientSetKeyboardCapture).toHaveBeenCalledExactlyOnceWith(42, true, captureArea);
  });

  it('arms system keys on pointer entry without a click or keyboard focus', async () => {
    vi.mocked(isTauri).mockReturnValue(true);
    dispatch(stage, 'pointerenter', pointer(200, 100));
    await vi.runAllTimersAsync();
    expect(api.clientSetKeyboardCapture).toHaveBeenCalledExactlyOnceWith(42, true, captureArea);
    expect(stage.focus).not.toHaveBeenCalled();
    expect(input.captureEnabled.value).toBe(true);
    // Keyboard ownership follows the pointer, including a visible background
    // window. Browser focus changes must not silently remove the hook.
    dispatch(window, 'blur');
    await vi.runAllTimersAsync();
    expect(api.clientSetKeyboardCapture).toHaveBeenCalledTimes(1);
    dispatch(stage, 'pointerleave');
    await vi.runAllTimersAsync();
    // Cursor ownership is checked natively; leaving must not uninstall the
    // hook and introduce a race before the next Win/Alt+Tab on pointer entry.
    expect(api.clientSetKeyboardCapture).toHaveBeenCalledTimes(1);
    expect(input.captureEnabled.value).toBe(true);
  });

  it('converts the remote surface to physical client coordinates for high DPI hit testing', async () => {
    vi.mocked(isTauri).mockReturnValue(true);
    Object.assign(window, { devicePixelRatio: 2 });
    dispatch(stage, 'pointerenter', pointer(200, 100));
    await vi.runAllTimersAsync();
    expect(api.clientSetKeyboardCapture).toHaveBeenLastCalledWith(42, true, {
      left: 200,
      top: 100,
      right: 1000,
      bottom: 500,
    });
    // Out-of-bounds pointer capture must not widen the native rectangle.
    dispatch(stage, 'pointermove', pointer(200, 20));
    await vi.runAllTimersAsync();
    expect(api.clientSetKeyboardCapture).toHaveBeenCalledTimes(1);
  });

  it('uninstalls native capture when the remote view is hidden', async () => {
    vi.mocked(isTauri).mockReturnValue(true);
    dispatch(stage, 'pointerenter', pointer(200, 100));
    await vi.runAllTimersAsync();
    Object.assign(document, { visibilityState: 'hidden' });
    dispatch(document, 'visibilitychange');
    await vi.runAllTimersAsync();
    expect(api.clientSetKeyboardCapture).toHaveBeenLastCalledWith(42, false, undefined);
    expect(input.captureEnabled.value).toBe(false);
  });

  it('shows the native installation error instead of claiming system keys are captured', async () => {
    vi.mocked(isTauri).mockReturnValue(true);
    vi.mocked(api.clientSetKeyboardCapture).mockRejectedValueOnce(
      new Error('SetWindowsHookEx: 1428'),
    );
    dispatch(stage, 'pointerenter', pointer(200, 100));
    await vi.runAllTimersAsync();
    expect(input.captureEnabled.value).toBe(false);
    expect(input.captureError.value).toContain('SetWindowsHookEx: 1428');
    dispatch(stage, 'keydown', { code: 'KeyA' });
    await vi.runAllTimersAsync();
    expect(sent()).toContainEqual({ type: 'key', scancode: 0x1e, extended: false, down: true });
    expect(input.captureError.value).toContain('SetWindowsHookEx: 1428');
  });

  it('retains DOM keyboard forwarding on platforms without native capture', async () => {
    vi.mocked(isTauri).mockReturnValue(true);
    vi.mocked(api.clientSetKeyboardCapture).mockResolvedValueOnce(false);
    dispatch(stage, 'mousedown', pointer(200, 100));
    await vi.runAllTimersAsync();
    dispatch(stage, 'keydown', { code: 'AltLeft' });
    dispatch(stage, 'keydown', { code: 'Tab' });
    dispatch(stage, 'keyup', { code: 'Tab' });
    dispatch(stage, 'keyup', { code: 'AltLeft' });
    await vi.runAllTimersAsync();
    expect(sent().filter((e) => e.type === 'key')).toEqual([
      { type: 'key', scancode: 0x38, extended: false, down: true },
      { type: 'key', scancode: 0x0f, extended: false, down: true },
      { type: 'key', scancode: 0x0f, extended: false, down: false },
      { type: 'key', scancode: 0x38, extended: false, down: false },
    ]);
  });

  it('positions a first click without requiring an earlier move', async () => {
    dispatch(stage, 'mousedown', pointer(200, 100));
    await vi.advanceTimersByTimeAsync(0);
    expect(sent()).toEqual([
      { type: 'mouseMove', x: 0.25, y: 0.25 },
      { type: 'mouseButton', button: 1, down: true },
    ]);
    expect(api.clientSendInputBatch).toHaveBeenCalledWith(expect.any(Array), 42);
  });

  it('forwards every button in a mouse chord and releases each exactly once', async () => {
    dispatch(stage, 'pointerdown', pointer(200, 100));
    dispatch(stage, 'mousedown', pointer(200, 100));
    dispatch(stage, 'mousedown', { ...pointer(200, 100), button: 2 });
    dispatch(stage, 'mouseup', pointer(200, 100));
    dispatch(stage, 'pointerup', { ...pointer(200, 100), button: 2 });
    dispatch(stage, 'mouseup', { ...pointer(200, 100), button: 2 });
    dispatch(stage, 'lostpointercapture');
    await vi.advanceTimersByTimeAsync(0);
    expect(sent().filter((e) => e.type === 'mouseButton')).toEqual([
      { type: 'mouseButton', button: 1, down: true },
      { type: 'mouseButton', button: 2, down: true },
      { type: 'mouseButton', button: 1, down: false },
      { type: 'mouseButton', button: 2, down: false },
    ]);
  });

  it('flushes the final drag position before blur releases without leaving a stale rAF', async () => {
    dispatch(stage, 'mousedown', pointer(200, 100));
    await vi.advanceTimersByTimeAsync(0);
    vi.mocked(api.clientSendInputBatch).mockClear();
    dispatch(stage, 'pointermove', pointer(400, 200));
    dispatch(window, 'blur');
    expect(vi.getTimerCount()).toBe(0);
    await vi.runAllTimersAsync();
    expect(sent()).toEqual([
      { type: 'mouseMove', x: 0.75, y: 0.75 },
      { type: 'mouseButton', button: 1, down: false },
    ]);
  });

  it('keeps held chord buttons down when capture is lost', async () => {
    dispatch(stage, 'mousedown', pointer(200, 100));
    dispatch(stage, 'mousedown', { ...pointer(200, 100), button: 2 });
    dispatch(stage, 'lostpointercapture', { buttons: 2 });
    await vi.advanceTimersByTimeAsync(0);
    expect(sent().filter((e) => e.type === 'mouseButton')).toEqual([
      { type: 'mouseButton', button: 1, down: true },
      { type: 'mouseButton', button: 2, down: true },
      { type: 'mouseButton', button: 1, down: false },
    ]);
    dispatch(window, 'mouseup', { ...pointer(200, 100), button: 2 });
    await vi.advanceTimersByTimeAsync(0);
    expect(sent().at(-1)).toEqual({ type: 'mouseButton', button: 2, down: false });
  });

  it('releases every held mouse button on pointercancel', async () => {
    dispatch(stage, 'mousedown', pointer(200, 100));
    dispatch(stage, 'mousedown', { ...pointer(200, 100), button: 2 });
    dispatch(stage, 'pointercancel', { button: -1, buttons: 3 });
    await vi.advanceTimersByTimeAsync(0);
    expect(sent().filter((e) => e.type === 'mouseButton' && !e.down)).toEqual([
      { type: 'mouseButton', button: 1, down: false },
      { type: 'mouseButton', button: 2, down: false },
    ]);
  });

  it('releases keys when focus moves from the stage to the toolbar', async () => {
    dispatch(stage, 'keydown', { code: 'ControlLeft' });
    dispatch(stage, 'focusout', { relatedTarget: null });
    await vi.runAllTimersAsync();
    expect(sent()).toEqual([
      { type: 'key', scancode: 29, extended: false, down: true },
      { type: 'key', scancode: 29, extended: false, down: false },
    ]);
  });

  it('positions the cursor before a wheel event and cancels any stale scheduled move', async () => {
    dispatch(stage, 'pointermove', pointer(200, 100));
    dispatch(stage, 'wheel', {
      clientX: 400,
      clientY: 200,
      deltaMode: 0,
      deltaX: 120,
      deltaY: 240,
    });
    await vi.advanceTimersByTimeAsync(20);
    expect(sent()).toEqual([
      { type: 'mouseMove', x: 0.75, y: 0.75 },
      { type: 'wheel', dx: 1, dy: -2 },
    ]);
  });

  it('uses release coordinates and cancels the stale move scheduled by rAF', async () => {
    dispatch(stage, 'mousedown', pointer(200, 100));
    await vi.advanceTimersByTimeAsync(0);
    vi.mocked(api.clientSendInputBatch).mockClear();
    dispatch(stage, 'pointermove', pointer(250, 110));
    dispatch(stage, 'mouseup', pointer(400, 200));
    await vi.advanceTimersByTimeAsync(20);
    expect(sent()).toEqual([
      { type: 'mouseMove', x: 0.75, y: 0.75 },
      { type: 'mouseButton', button: 1, down: false },
    ]);
  });

  it('flushes releases at stop with no timers or duplicate releases left', async () => {
    dispatch(stage, 'keydown', { code: 'KeyA' });
    dispatch(stage, 'mousedown', pointer(200, 100));
    await vi.advanceTimersByTimeAsync(0);
    vi.mocked(api.clientSendInputBatch).mockClear();
    input.stop();
    expect(vi.getTimerCount()).toBe(0);
    await Promise.resolve();
    await Promise.resolve();
    expect(sent()).toEqual([
      { type: 'mouseButton', button: 1, down: false },
      { type: 'key', scancode: 30, extended: false, down: false },
    ]);
    input.stop();
    dispatch(stage, 'keydown', { code: 'KeyA' });
    await vi.runAllTimersAsync();
    expect(sent()).toHaveLength(2);
  });

  it('clears the batch timer when the size limit forces an early flush', async () => {
    for (let i = 0; i < 64; i++) dispatch(stage, 'keydown', { code: 'KeyA' });
    expect(vi.getTimerCount()).toBe(0);
    await vi.runAllTimersAsync();
    expect(api.clientSendInputBatch).toHaveBeenCalledTimes(1);
    expect(sent()).toHaveLength(64);
  });

  it('releases held keys on blur while preserving auto-repeat', async () => {
    dispatch(stage, 'keydown', { code: 'KeyA' });
    dispatch(stage, 'keydown', { code: 'KeyA' });
    dispatch(window, 'blur');
    dispatch(stage, 'keyup', { code: 'KeyA' });
    await vi.advanceTimersByTimeAsync(0);
    expect(sent().map((event) => event.type === 'key' && event.down)).toEqual([true, true, false]);
  });

  it.each([
    ['PrintScreen', 0x37, true],
    ['ContextMenu', 0x5d, true],
    ['F24', 0x76, false],
    ['NumpadEqual', 0x59, false],
    ['MediaPlayPause', 0x22, true],
  ] as const)('forwards both transitions of %s', async (code, scancode, extended) => {
    dispatch(stage, 'keydown', { code });
    dispatch(stage, 'keyup', { code });
    await vi.advanceTimersByTimeAsync(0);
    expect(sent()).toEqual([
      { type: 'key', scancode, extended, down: true },
      { type: 'key', scancode, extended, down: false },
    ]);
  });

  it.each(['Pause', 'Unidentified'])(
    'reports unsupported %s and suppresses local defaults',
    async (code) => {
      const down = new Event('keydown', { cancelable: true });
      Object.assign(down, { code });
      stage.dispatchEvent(down);
      const up = new Event('keyup', { cancelable: true });
      Object.assign(up, { code });
      stage.dispatchEvent(up);
      await vi.advanceTimersByTimeAsync(0);
      expect(down.defaultPrevented).toBe(true);
      expect(up.defaultPrevented).toBe(true);
      expect(sent()).toEqual([]);
      expect(input.unsupportedKey.value).toContain(code);
      dispatch(stage, 'keydown', { code: 'KeyA' });
      expect(input.unsupportedKey.value).toBe('');
    },
  );

  it('ends the session if reliable input fails and does not send later batches', async () => {
    vi.mocked(api.clientSendInputBatch).mockRejectedValueOnce(new Error('IPC failed'));
    dispatch(stage, 'keydown', { code: 'KeyA' });
    await vi.advanceTimersByTimeAsync(0);
    dispatch(stage, 'keyup', { code: 'KeyA' });
    await vi.advanceTimersByTimeAsync(0);
    expect(api.clientDisconnect).toHaveBeenCalledExactlyOnceWith(42);
    expect(api.clientSendInputBatch).toHaveBeenCalledTimes(1);
  });

  it('clamps captured drags outside the canvas to its edges', async () => {
    dispatch(stage, 'pointermove', pointer(-50, 600));
    await vi.advanceTimersByTimeAsync(20);
    expect(sent()).toEqual([{ type: 'mouseMove', x: 0, y: 1 }]);
  });

  it('does not send non-finite coordinates when the canvas is hidden', async () => {
    rect.width = 0;
    rect.height = 0;
    dispatch(stage, 'pointermove', pointer(200, 100));
    await vi.advanceTimersByTimeAsync(20);
    expect(sent()).toEqual([]);
  });

  it('coalesces adjacent moves without merging across a reliable key event', async () => {
    let raf = () => {};
    vi.stubGlobal('requestAnimationFrame', (cb: () => void) => {
      raf = cb;
      return 1;
    });
    vi.stubGlobal('cancelAnimationFrame', vi.fn());
    dispatch(stage, 'pointermove', pointer(150, 75));
    raf();
    dispatch(stage, 'pointermove', pointer(300, 150));
    raf();
    dispatch(stage, 'keydown', { code: 'KeyA' });
    dispatch(stage, 'pointermove', pointer(400, 200));
    raf();
    await vi.advanceTimersByTimeAsync(0);
    expect(sent()).toEqual([
      { type: 'mouseMove', x: 0.5, y: 0.5 },
      { type: 'key', scancode: 30, extended: false, down: true },
      { type: 'mouseMove', x: 0.75, y: 0.75 },
    ]);
  });

  it('keeps later batches behind an in-flight reliable send', async () => {
    let resolveSend = () => {};
    vi.mocked(api.clientSendInputBatch).mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          resolveSend = resolve;
        }),
    );
    dispatch(stage, 'keydown', { code: 'KeyA' });
    await vi.advanceTimersByTimeAsync(0);
    dispatch(stage, 'keyup', { code: 'KeyA' });
    await vi.advanceTimersByTimeAsync(0);
    expect(api.clientSendInputBatch).toHaveBeenCalledTimes(1);
    resolveSend();
    await vi.runAllTimersAsync();
    expect(api.clientSendInputBatch).toHaveBeenCalledTimes(2);
    expect(sent().map((event) => event.type === 'key' && event.down)).toEqual([true, false]);
  });
  it('stamps native capture and queued mouse input with the displayed child screen', async () => {
    input.stop();
    vi.mocked(isTauri).mockReturnValue(true);
    const target = { streamId: 2, generation: 4, revision: 7 };
    input = useInput(
      () => ({ getBoundingClientRect: () => rect }) as HTMLCanvasElement,
      42,
      () => target,
    );
    input.bind(stage as unknown as HTMLElement);
    dispatch(stage, 'mousedown', pointer(200, 100));
    dispatch(stage, 'mouseup', pointer(200, 100));
    await vi.runAllTimersAsync();
    expect(api.clientSetKeyboardCapture).toHaveBeenCalledWith(42, true, captureArea, target);
    expect(api.clientSendInputBatch).toHaveBeenCalledWith(expect.any(Array), 42, target);
  });
});
