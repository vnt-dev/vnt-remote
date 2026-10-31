import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { effectScope, nextTick, shallowRef, type ShallowRef } from 'vue';
import { useDeviceSort } from './useDeviceSort';

vi.mock('vue', async (original) => ({
  ...(await original<typeof import('vue')>()),
  onUnmounted: vi.fn(),
}));

function dispatch(target: EventTarget, type: string, fields: Record<string, unknown> = {}) {
  const event = new Event(type, { cancelable: true });
  Object.assign(event, fields);
  target.dispatchEvent(event);
}

describe('device pointer sorting', () => {
  let scope: ReturnType<typeof effectScope>;
  let sort: ReturnType<typeof useDeviceSort<{ id: string }>>;
  let devices: ShallowRef<{ id: string }[]>;
  let windowTarget: EventTarget;
  let frames: Map<number, FrameRequestCallback>;
  let floating: { remove: ReturnType<typeof vi.fn> };
  let scroll: { scrollTop: number; getBoundingClientRect: () => object };
  const bodyClasses = new Set<string>();
  const pointer = (x = 290, y = 90, pointerId = 1) => ({
    clientX: x,
    clientY: y,
    pointerId,
    button: 0,
    isPrimary: true,
  });
  const order = () => sort.sortedDevices.value.map((device) => device.id);
  function runFrame() {
    const callbacks = [...frames.values()];
    frames.clear();
    callbacks.forEach((callback) => callback(16));
  }
  function start(button = 0) {
    const event = new Event('pointerdown', { cancelable: true });
    const card = cards[0];
    Object.assign(event, pointer(), { button });
    Object.defineProperty(event, 'currentTarget', { value: { closest: () => card } });
    sort.start('a', event as PointerEvent);
  }
  let cards: HTMLElement[];

  beforeEach(() => {
    bodyClasses.clear();
    frames = new Map();
    let frameId = 0;
    windowTarget = new EventTarget();
    Object.assign(windowTarget, { matchMedia: () => ({ matches: false }) });
    vi.stubGlobal('window', windowTarget);
    vi.stubGlobal(
      'document',
      Object.assign(new EventTarget(), {
        body: {
          classList: {
            add: (name: string) => bodyClasses.add(name),
            remove: (name: string) => bodyClasses.delete(name),
          },
          append: vi.fn(),
        },
      }),
    );
    vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
      frames.set(++frameId, callback);
      return frameId;
    });
    vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id));
    floating = {
      remove: vi.fn(),
      ...{
        classList: { add: vi.fn() },
        setAttribute: vi.fn(),
        style: {},
        getBoundingClientRect: () => ({ left: 0, top: 0 }),
        animate: () => {
          const animation = { onfinish: null as (() => void) | null };
          queueMicrotask(() => animation.onfinish?.());
          return animation;
        },
      },
    };
    devices = shallowRef(['a', 'b', 'c', 'd'].map((id) => ({ id })));
    scope = effectScope();
    sort = scope.run(() => useDeviceSort(devices))!;
    cards = devices.value.map((device) => ({
      get offsetLeft() {
        return (order().indexOf(device.id) % 3) * 316;
      },
      get offsetTop() {
        return Math.floor(order().indexOf(device.id) / 3) * 116;
      },
      offsetWidth: 300,
      offsetHeight: 100,
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 300, height: 100 }),
      cloneNode: () => floating,
    })) as unknown as HTMLElement[];
    scroll = {
      scrollTop: 0,
      getBoundingClientRect: () => ({ left: 0, right: 1000, top: 0, bottom: 400 }),
    };
    sort.grid.value = {
      getBoundingClientRect: () => ({ left: 0, right: 1000, top: 0, bottom: 600 }),
      querySelectorAll: () =>
        order().map((id) => cards[devices.value.findIndex((d) => d.id === id)]),
      closest: () => scroll,
    } as unknown as HTMLElement;
  });

  afterEach(() => {
    sort.cancel();
    scope.stop();
    vi.unstubAllGlobals();
  });

  it('previews across rows without persisting, then commits on pointer release', async () => {
    start();
    dispatch(windowTarget, 'pointermove', pointer(290, 206));
    runFrame();
    expect(order()).toEqual(['b', 'c', 'd', 'a']);
    expect(devices.value.map((d) => d.id)).toEqual(['a', 'b', 'c', 'd']);
    expect(sort.draggedId.value).toBe('a');
    dispatch(windowTarget, 'pointerup', pointer(290, 206));
    await nextTick();
    expect(devices.value.map((d) => d.id)).toEqual(['b', 'c', 'd', 'a']);
    expect(sort.draggedId.value).toBe('');
    expect(bodyClasses.size).toBe(0);
    expect(frames.size).toBe(0);
  });

  it.each(['escape', 'blur', 'pointercancel'])('restores the saved order on %s', (reason) => {
    start();
    dispatch(windowTarget, 'pointermove', pointer(606, 90));
    runFrame();
    expect(order()).toEqual(['b', 'a', 'c', 'd']);
    dispatch(windowTarget, reason === 'escape' ? 'keydown' : reason, { key: 'Escape' });
    expect(order()).toEqual(['a', 'b', 'c', 'd']);
    expect(floating.remove).toHaveBeenCalled();
    expect(frames.size).toBe(0);
    expect(bodyClasses.size).toBe(0);
  });

  it('treats a click or a different pointer as no reorder', () => {
    start();
    dispatch(windowTarget, 'pointermove', pointer(900, 90, 2));
    dispatch(windowTarget, 'pointermove', pointer(292, 90));
    expect(sort.draggedId.value).toBe('');
    dispatch(windowTarget, 'pointerup', pointer(292, 90));
    expect(order()).toEqual(['a', 'b', 'c', 'd']);
    expect(bodyClasses.size).toBe(0);
    start(2);
    expect(bodyClasses.size).toBe(0);
  });

  it('uses the final pointer position even before the next animation frame', async () => {
    start();
    dispatch(windowTarget, 'pointermove', pointer(606, 90));
    dispatch(windowTarget, 'pointerup', pointer(922, 90));
    await nextTick();
    expect(order()).toEqual(['b', 'c', 'a', 'd']);
    expect(frames.size).toBe(0);
  });

  it('scrolls near the pane edge and stops scrolling after cancellation', () => {
    start();
    dispatch(windowTarget, 'pointermove', pointer(290, 390));
    runFrame();
    expect(scroll.scrollTop).toBeGreaterThan(0);
    sort.cancel();
    const position = scroll.scrollTop;
    runFrame();
    expect(scroll.scrollTop).toBe(position);
  });
});
