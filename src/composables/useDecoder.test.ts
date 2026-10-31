import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ref } from 'vue';
import { useDecoder } from './useDecoder';
import { api } from '../services/api';

vi.mock('../services/api', () => ({
  api: {
    clientRequestKeyframe: vi.fn().mockResolvedValue(undefined),
    clientDisconnect: vi.fn().mockResolvedValue(undefined),
  },
}));

class Socket {
  static instances: Socket[] = [];
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((event: { data: ArrayBuffer | string }) => void) | null = null;
  onerror = null;
  binaryType = '';
  constructor() {
    Socket.instances.push(this);
  }
  close() {
    this.onclose?.();
  }
}
async function settleDecoder() {
  // Allow capability probes and decoder configuration to finish before sending media.
  for (let i = 0; i < 8; i++) await Promise.resolve();
}
class Decoder {
  static isConfigSupported = vi
    .fn<(config: VideoDecoderConfig) => Promise<{ supported: boolean }>>()
    .mockResolvedValue({ supported: true });
  static instances: Decoder[] = [];
  decodeQueueSize = 0;
  decode = vi.fn();
  configure = vi.fn();
  close = vi.fn();
  error: (error: DOMException) => void;
  output: (frame: { timestamp: number; close: () => void }) => void = () => {};
  constructor(init: {
    output: (frame: { timestamp: number; close: () => void }) => void;
    error: (error: DOMException) => void;
  }) {
    this.output = init.output;
    this.error = init.error;
    Decoder.instances.push(this);
  }
}
const initial = { codec: 'h264', width: 64, height: 48, fps: 30, hardware: false };
let rafCallbacks: Array<() => void> = [];

/** 构造一帧二进制消息（[u8 keyframe][u32 pts][u32 len][data]） */
function frameMsg(keyframe: number, pts: number): ArrayBuffer {
  const buf = new ArrayBuffer(10);
  const v = new DataView(buf);
  v.setUint8(0, keyframe);
  v.setUint32(1, pts);
  v.setUint32(5, 1);
  return buf;
}

describe('video recovery', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    Socket.instances = [];
    Decoder.instances = [];
    Decoder.isConfigSupported.mockResolvedValue({ supported: true });
    vi.stubGlobal('WebSocket', Socket);
    vi.stubGlobal('VideoDecoder', Decoder);
    vi.stubGlobal(
      'EncodedVideoChunk',
      class {
        constructor(public init: unknown) {}
      },
    );
    rafCallbacks = [];
    vi.stubGlobal(
      'requestAnimationFrame',
      vi.fn((cb: () => void) => {
        rafCallbacks.push(cb);
        return rafCallbacks.length;
      }),
    );
    vi.stubGlobal('cancelAnimationFrame', vi.fn());
  });
  afterEach(() => {
    vi.restoreAllMocks();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('selects supported software before configuring when hardware is unavailable', async () => {
    Decoder.isConfigSupported.mockImplementation(async (config) => ({
      supported: config.hardwareAcceleration === 'prefer-software',
    }));
    const fatal = vi.fn();
    const decoder = useDecoder(ref(), fatal);
    decoder.connect(1234, 'token', initial);
    decoder.handleMessage(frameMsg(1, 1));
    expect(Decoder.instances).toHaveLength(0);
    await settleDecoder();
    expect(
      Decoder.isConfigSupported.mock.calls.map(([config]) => config.hardwareAcceleration),
    ).toEqual(['prefer-hardware', 'prefer-software']);
    expect(Decoder.instances).toHaveLength(1);
    expect(Decoder.instances[0].configure).toHaveBeenCalledWith(
      expect.objectContaining({
        hardwareAcceleration: 'prefer-software',
        codedWidth: 64,
        codedHeight: 48,
      }),
    );
    expect(decoder.lastError.value).toBe('');
    expect(fatal).not.toHaveBeenCalled();
    expect(api.clientRequestKeyframe).toHaveBeenCalled();
    decoder.handleMessage(frameMsg(0, 2));
    expect(Decoder.instances[0].decode).not.toHaveBeenCalled();
    decoder.handleMessage(frameMsg(1, 3));
    expect(Decoder.instances[0].decode).toHaveBeenCalledOnce();
    decoder.close();
  });

  it('ignores a capability probe completed after close', async () => {
    let finish!: (support: { supported: boolean }) => void;
    Decoder.isConfigSupported.mockReturnValueOnce(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    decoder.close();
    finish({ supported: true });
    await settleDecoder();
    expect(Decoder.instances).toHaveLength(0);
    expect(api.clientRequestKeyframe).not.toHaveBeenCalled();
    expect(vi.getTimerCount()).toBe(0);
  });

  it('does not apply a stale capability result after the stream changes', async () => {
    let finish!: (support: { supported: boolean }) => void;
    Decoder.isConfigSupported.mockReturnValueOnce(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    decoder.handleMessage(JSON.stringify({ type: 'streamInfo', ...initial, width: 128 }));
    await settleDecoder();
    finish({ supported: true });
    await settleDecoder();
    expect(Decoder.instances).toHaveLength(1);
    expect(Decoder.instances[0].configure).toHaveBeenCalledWith(
      expect.objectContaining({ codedWidth: 128 }),
    );
    decoder.close();
  });

  it('clears a recovered decoder error on the first output frame', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    Decoder.instances[0].error(new DOMException('GPU failed'));
    await settleDecoder();
    expect(decoder.lastError.value).toContain('GPU failed');
    Decoder.instances.at(-1)!.output({ timestamp: 1000, close: vi.fn() });
    expect(decoder.lastError.value).toBe('');
    decoder.close();
  });

  it('reports unsupported hardware and software without configuring a decoder', async () => {
    Decoder.isConfigSupported.mockResolvedValue({ supported: false });
    const fatal = vi.fn();
    const decoder = useDecoder(ref(), fatal);
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    expect(Decoder.instances).toHaveLength(0);
    expect(fatal).toHaveBeenCalledOnce();
    expect(api.clientDisconnect).toHaveBeenCalledOnce();
    expect(vi.getTimerCount()).toBe(0);
  });

  it('ignores shared bridge messages after fatal failure until explicitly reconnected', async () => {
    const fatal = vi.fn();
    const decoder = useDecoder(ref(), fatal, 42, {
      streamId: 1,
      generation: () => 1,
      active: () => true,
    });
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    Decoder.instances.at(-1)!.error(new DOMException('hardware failed'));
    await settleDecoder();
    Decoder.instances.at(-1)!.error(new DOMException('software failed'));
    await settleDecoder();
    expect(fatal).toHaveBeenCalledOnce();
    const count = Decoder.instances.length;

    // Other screens keep the shared socket alive during this screen's retry delay.
    decoder.handleMessage(JSON.stringify({ type: 'streamInfo', ...initial }));
    await settleDecoder();
    decoder.handleMessage(JSON.stringify({ type: 'videoReset' }));
    await settleDecoder();
    decoder.handleMessage(frameMsg(1, 1));
    await settleDecoder();
    expect(Decoder.instances).toHaveLength(count);
    expect(Decoder.instances.at(-1)!.decode).not.toHaveBeenCalled();

    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    decoder.handleMessage(frameMsg(1, 2));
    await settleDecoder();
    expect(Decoder.instances).toHaveLength(count + 1);
    expect(Decoder.instances.at(-1)!.decode).toHaveBeenCalledOnce();
    decoder.close();
  });

  it('keeps healthy media during control recovery and resets only a broken media lane', async () => {
    const canvas = {
      width: 64,
      height: 48,
      getContext: () => ({ clearRect: vi.fn(), drawImage: vi.fn() }),
    };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const socket = Socket.instances[0];
    const previous = Decoder.instances[0];
    socket.onmessage?.({
      data: JSON.stringify({ type: 'transportState', kind: 0, available: false }),
    });
    await settleDecoder();
    expect(decoder.transport.value.control).toBe(false);
    expect(previous.close).not.toHaveBeenCalled();
    socket.onmessage?.({ data: frameMsg(1, 1) });
    await settleDecoder();
    expect(previous.decode).toHaveBeenCalledOnce();
    socket.onmessage?.({
      data: JSON.stringify({ type: 'transportState', kind: 2, available: false }),
    });
    await settleDecoder();
    expect(previous.close).toHaveBeenCalledOnce();
    socket.onmessage?.({
      data: JSON.stringify({ type: 'transportState', kind: 2, available: true }),
    });
    await settleDecoder();
    const resumed = Decoder.instances.at(-1)!;
    socket.onmessage?.({ data: frameMsg(0, 2) });
    await settleDecoder();
    expect(resumed.decode).not.toHaveBeenCalled();
    socket.onmessage?.({ data: frameMsg(1, 3) });
    await settleDecoder();
    expect(resumed.decode).toHaveBeenCalledOnce();
    expect(api.clientDisconnect).not.toHaveBeenCalled();
    decoder.close();
  });

  it('disposes the previous desktop and ignores frames until a fresh desktop keyframe', async () => {
    const clearRect = vi.fn();
    const canvas = { width: 64, height: 48, getContext: () => ({ clearRect, drawImage: vi.fn() }) };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    const previous = Decoder.instances[0];
    const pending = { timestamp: 0, close: vi.fn() };
    previous.output(pending);
    const socket = Socket.instances[0];
    socket.onmessage?.({
      data: JSON.stringify({ type: 'desktopState', ready: false, reason: '切换中' }),
    });
    await settleDecoder();
    expect(previous.close).toHaveBeenCalledOnce();
    expect(pending.close).toHaveBeenCalledOnce();
    expect(clearRect).toHaveBeenCalledWith(0, 0, 64, 48);
    socket.onmessage?.({ data: frameMsg(1, 1) });
    await settleDecoder();
    expect(previous.decode).not.toHaveBeenCalled();
    socket.onmessage?.({
      data: JSON.stringify({ type: 'streamInfo', ...initial, desktopReady: false }),
    });
    await settleDecoder();
    socket.onmessage?.({ data: JSON.stringify({ type: 'desktopState', ready: true }) });
    await settleDecoder();
    const resumed = Decoder.instances.at(-1)!;
    socket.onmessage?.({ data: frameMsg(0, 2) });
    await settleDecoder();
    expect(resumed.decode).not.toHaveBeenCalled();
    socket.onmessage?.({ data: frameMsg(1, 3) });
    await settleDecoder();
    expect(resumed.decode).toHaveBeenCalledOnce();
    decoder.close();
  });

  it('releases the desktop veil on the first painted frame without waiting for statistics', async () => {
    const clearRect = vi.fn();
    const drawImage = vi.fn();
    const canvas = { width: 64, height: 48, getContext: () => ({ clearRect, drawImage }) };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    expect(decoder.frameReady.value).toBe(false);
    const old = Decoder.instances[0];
    old.output({ timestamp: 0, close: vi.fn() });
    rafCallbacks.at(-1)?.();
    expect(decoder.frameReady.value).toBe(true);
    expect(decoder.stats.value.frames).toBe(0);
    const socket = Socket.instances[0];
    socket.onmessage?.({ data: JSON.stringify({ type: 'desktopState', ready: false }) });
    await settleDecoder();
    expect(decoder.frameReady.value).toBe(false);
    expect(clearRect).toHaveBeenCalled();
    const stale = { timestamp: 1, close: vi.fn() };
    old.output(stale);
    expect(stale.close).toHaveBeenCalledOnce();
    socket.onmessage?.({ data: JSON.stringify({ type: 'desktopState', ready: true }) });
    await settleDecoder();
    expect(decoder.frameReady.value).toBe(false);
    Decoder.instances.at(-1)?.output({ timestamp: 2000, close: vi.fn() });
    expect(decoder.frameReady.value).toBe(false);
    rafCallbacks.at(-1)?.();
    expect(decoder.frameReady.value).toBe(true);
    expect(drawImage).toHaveBeenCalledTimes(2);
    decoder.close();
  });

  it('clears an old desktop when a resumed stream snapshot reports a pending switch', async () => {
    const clearRect = vi.fn();
    const canvas = { width: 64, height: 48, getContext: () => ({ clearRect, drawImage: vi.fn() }) };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    Decoder.instances[0].output({ timestamp: 0, close: vi.fn() });
    rafCallbacks.at(-1)?.();
    expect(decoder.frameReady.value).toBe(true);
    Socket.instances[0].onmessage?.({
      data: JSON.stringify({ type: 'streamInfo', ...initial, desktopReady: false }),
    });
    await settleDecoder();
    expect(decoder.frameReady.value).toBe(false);
    expect(clearRect).toHaveBeenCalled();
    decoder.close();
  });

  it('shows SAS status and errors without interrupting an active desktop stream', async () => {
    const canvas = { width: 64, height: 48, getContext: () => ({ drawImage: vi.fn() }) };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    const active = Decoder.instances[0];
    const socket = Socket.instances[0];
    for (const reason of ['正在等待 Windows 安全界面', '系统策略禁止发送 Ctrl+Alt+Del', '']) {
      socket.onmessage?.({ data: JSON.stringify({ type: 'desktopState', ready: true, reason }) });
      await settleDecoder();
      expect(decoder.desktop.value).toEqual({ ready: true, reason });
      expect(active.close).not.toHaveBeenCalled();
    }
    decoder.close();
  });

  it('fills the negotiated canvas even when the driver reports a smaller display size', async () => {
    const drawImage = vi.fn();
    const canvas = { width: 0, height: 0, getContext: () => ({ drawImage }) };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', { ...initial, width: 3840, height: 2160 });
    await settleDecoder();
    const frame = { timestamp: 0, displayWidth: 1280, displayHeight: 720, close: vi.fn() };
    Decoder.instances[0].output(frame);
    rafCallbacks.at(-1)?.();
    expect(drawImage).toHaveBeenCalledWith(frame, 0, 0, 3840, 2160);
    expect(frame.close).toHaveBeenCalledOnce();
    decoder.close();
  });

  it.each(['videoReset', 'streamInfo', 'backlog', 'decodeError'])(
    'keeps the last screen frame visible during %s recovery and clears it on desktop changes',
    async (type) => {
      const clearRect = vi.fn();
      const drawImage = vi.fn();
      const canvas = { width: 64, height: 48, getContext: () => ({ clearRect, drawImage }) };
      let active = true;
      let generation = 1;
      const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement), undefined, 42, {
        streamId: 1,
        generation: () => generation,
        active: () => active,
      });
      decoder.connect(1234, 'token', initial);
      await settleDecoder();
      const old = Decoder.instances.at(-1)!;
      const displayed = { timestamp: 1000, close: vi.fn() };
      old.output(displayed);
      rafCallbacks.at(-1)?.();
      clearRect.mockClear();
      if (type === 'backlog') {
        old.decodeQueueSize = 9;
        decoder.handleMessage(frameMsg(1, 2));
        await settleDecoder();
      } else if (type === 'decodeError') {
        old.error(new DOMException('GPU decode failed'));
        await settleDecoder();
      } else {
        decoder.handleMessage(JSON.stringify({ ...initial, type }));
        await settleDecoder();
      }
      expect(old.close).toHaveBeenCalledOnce();
      expect(decoder.frameReady.value).toBe(true);
      expect(clearRect).not.toHaveBeenCalled();
      expect(displayed.close).not.toHaveBeenCalled();
      const stale = { timestamp: 2000, close: vi.fn() };
      old.output(stale);
      expect(stale.close).toHaveBeenCalledOnce();
      active = false;
      rafCallbacks.at(-1)?.();
      active = true;
      rafCallbacks.at(-1)?.();
      expect(drawImage).toHaveBeenLastCalledWith(displayed, 0, 0, 64, 48);

      const fresh = { timestamp: 3000, close: vi.fn() };
      Decoder.instances.at(-1)!.output(fresh);
      expect(displayed.close).toHaveBeenCalledOnce();
      rafCallbacks.at(-1)?.();
      expect(drawImage).toHaveBeenLastCalledWith(fresh, 0, 0, 64, 48);

      generation++;
      decoder.handleMessage(JSON.stringify({ ...initial, type: 'streamInfo' }));
      await settleDecoder();
      expect(decoder.frameReady.value).toBe(false);
      expect(fresh.close).toHaveBeenCalledOnce();
      expect(clearRect).toHaveBeenCalledOnce();
      Decoder.instances.at(-1)!.output({ timestamp: 4000, close: vi.fn() });
      decoder.handleMessage(JSON.stringify({ type: 'desktopState', ready: false }));
      await settleDecoder();
      expect(decoder.frameReady.value).toBe(false);
      decoder.close();
    },
  );

  it('counts only unpainted replacements as dropped in a retained screen stream', async () => {
    const canvas = {
      width: 64,
      height: 48,
      getContext: () => ({ drawImage: vi.fn(), clearRect: vi.fn() }),
    };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement), undefined, 42, {
      streamId: 1,
      generation: () => 1,
      active: () => true,
    });
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    const current = Decoder.instances.at(-1)!;
    current.output({ timestamp: 1000, close: vi.fn() });
    rafCallbacks.at(-1)?.();
    current.output({ timestamp: 2000, close: vi.fn() });
    current.output({ timestamp: 3000, close: vi.fn() });
    rafCallbacks.at(-1)?.();
    await vi.advanceTimersByTimeAsync(1000);
    expect(decoder.stats.value.frames).toBe(3);
    expect(decoder.stats.value.displayed).toBe(2);
    expect(decoder.stats.value.droppedDecoded).toBe(1);
    decoder.close();
  });

  it('paints a 4K preview at visible device pixels and restores full detail when enlarged', async () => {
    vi.stubGlobal('devicePixelRatio', 2);
    const drawImage = vi.fn();
    const parent = { clientWidth: 1280, clientHeight: 720 };
    const canvas = {
      width: 0,
      height: 0,
      style: { width: '', height: '' },
      parentElement: parent,
      getContext: () => ({ drawImage }),
    };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', { ...initial, width: 3840, height: 2160 });
    await settleDecoder();
    const first = { timestamp: 1000, close: vi.fn() };
    Decoder.instances[0].output(first);
    rafCallbacks.at(-1)?.();
    expect(canvas.style).toEqual({ width: '1280px', height: '720px' });
    expect(drawImage).toHaveBeenLastCalledWith(first, 0, 0, 2560, 1440);
    expect(decoder.streamInfo.value).toMatchObject({ width: 3840, height: 2160 });
    parent.clientWidth = 1920;
    parent.clientHeight = 1080;
    const second = { timestamp: 2000, close: vi.fn() };
    Decoder.instances[0].output(second);
    rafCallbacks.at(-1)?.();
    expect(drawImage).toHaveBeenLastCalledWith(second, 0, 0, 3840, 2160);
    expect(canvas.style).toEqual({ width: '1920px', height: '1080px' });
    decoder.close();
  });

  it('reports unavailable WebCodecs without throwing or leaving timers behind', async () => {
    vi.stubGlobal('VideoDecoder', undefined);
    const fatal = vi.fn();
    const decoder = useDecoder(ref(), fatal, 42);
    expect(() => decoder.connect(1234, 'token', initial)).not.toThrow();
    await settleDecoder();
    expect(fatal).toHaveBeenCalledOnce();
    expect(api.clientDisconnect).toHaveBeenCalledExactlyOnceWith(42);
    expect(Socket.instances).toHaveLength(0);
    expect(vi.getTimerCount()).toBe(0);
  });

  it.each(['null', 'throws'])('ends the session when the canvas context %s', async (failure) => {
    const canvas = {
      width: 0,
      height: 0,
      getContext: () => {
        if (failure === 'throws') throw new Error('context allocation failed');
        return null;
      },
    };
    const fatal = vi.fn();
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement), fatal, 42);
    expect(() => decoder.connect(1234, 'token', initial)).not.toThrow();
    await settleDecoder();
    expect(fatal).toHaveBeenCalledOnce();
    expect(api.clientDisconnect).toHaveBeenCalledExactlyOnceWith(42);
    expect(Socket.instances).toHaveLength(0);
    expect(vi.getTimerCount()).toBe(0);
  });

  it('requests a fresh keyframe after stream reset and reconnects with the latest dimensions', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const socket = Socket.instances[0];
    socket.onmessage?.({ data: frameMsg(1, 0) });
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    socket.onmessage?.({
      data: JSON.stringify({ ...initial, type: 'streamInfo', width: 128, height: 96 }),
    });
    await settleDecoder();
    expect(api.clientRequestKeyframe).toHaveBeenCalledTimes(2);
    socket.onclose?.();
    await vi.advanceTimersByTimeAsync(1000);
    expect(Decoder.instances.at(-1)!.configure.mock.calls[0][0]).toMatchObject({
      codedWidth: 128,
      codedHeight: 96,
    });
    expect(socket.onmessage).toBeNull();
    decoder.close();
  });

  it('replaces an existing socket without letting stale messages mutate the decoder', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'first', initial);
    await settleDecoder();
    const old = Socket.instances[0];
    decoder.connect(1235, 'second', initial);
    await settleDecoder();
    expect(old.onmessage).toBeNull();
    expect(old.onclose).toBeNull();
    decoder.close();
  });

  it.each(['videoReset', 'streamInfo'])(
    'recovers bridge backlog via %s on the same socket',
    async (type) => {
      const fatal = vi.fn();
      const decoder = useDecoder(ref(), fatal, 42);
      decoder.connect(1234, 'token', initial);
      await settleDecoder();
      const socket = Socket.instances[0];
      socket.onmessage?.({ data: frameMsg(1, 0) });
      await settleDecoder();
      const old = Decoder.instances[0];
      vi.mocked(api.clientRequestKeyframe).mockClear();
      socket.onmessage?.({ data: JSON.stringify({ ...initial, type }) });
      await settleDecoder();
      const current = Decoder.instances.at(-1)!;
      expect(old.close).toHaveBeenCalledOnce();
      expect(current).not.toBe(old);
      expect(api.clientRequestKeyframe).toHaveBeenCalledOnce();
      socket.onmessage?.({ data: frameMsg(0, 100) });
      await settleDecoder();
      expect(current.decode).not.toHaveBeenCalled();
      socket.onmessage?.({ data: frameMsg(1, 200) });
      await settleDecoder();
      socket.onmessage?.({ data: frameMsg(0, 233) });
      await settleDecoder();
      expect(current.decode).toHaveBeenCalledTimes(2);
      expect(Socket.instances).toHaveLength(1);
      expect(decoder.lastError.value).toBe('');
      expect(fatal).not.toHaveBeenCalled();
      decoder.close();
    },
  );

  it('tries software decoding once, ignores stale callbacks, and then reports failure', async () => {
    const fatal = vi.fn();
    const decoder = useDecoder(ref(), fatal);
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    const first = Decoder.instances[0];
    // 硬件配置通过能力探测后才使用 prefer-hardware。
    expect(first.configure.mock.calls[0][0].hardwareAcceleration).toBe('prefer-hardware');
    first.error(new DOMException('GPU decode failed'));
    await settleDecoder();
    const software = Decoder.instances.at(-1)!;
    expect(software.configure.mock.calls[0][0].hardwareAcceleration).toBe('prefer-software');
    const staleFrame = { timestamp: 0, close: vi.fn() };
    first.output(staleFrame);
    first.error(new DOMException('late error'));
    await settleDecoder();
    expect(staleFrame.close).toHaveBeenCalledOnce();
    expect(fatal).not.toHaveBeenCalled();
    software.error(new DOMException('software unavailable'));
    await settleDecoder();
    expect(fatal).toHaveBeenCalledOnce();
    expect(api.clientDisconnect).toHaveBeenCalledOnce();
    expect(vi.getTimerCount()).toBe(0);
  });

  it('detects a silent decoder stall even when backlog recovery keeps rebuilding it', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const socket = Socket.instances[0];
    for (let i = 0; i < 40; i++) {
      socket.onmessage?.({ data: frameMsg(1, i * 200) });
      await settleDecoder();
      socket.onmessage?.({ data: frameMsg(0, i * 200 + 150) });
      await settleDecoder();
      await vi.advanceTimersByTimeAsync(100);
    }
    expect(Decoder.instances.at(-1)!.configure.mock.calls[0][0].hardwareAcceleration).toBe(
      'prefer-software',
    );
    expect(api.clientRequestKeyframe).toHaveBeenCalled();
    decoder.close();
  });

  it('ends a session after repeated open/close cycles without video', async () => {
    const fatal = vi.fn();
    const decoder = useDecoder(ref(), fatal);
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    for (let i = 0; i < 6; i++) {
      const socket = Socket.instances.at(-1)!;
      socket.onopen?.();
      await settleDecoder();
      socket.onclose?.();
      await vi.advanceTimersByTimeAsync(1000);
    }
    expect(Socket.instances).toHaveLength(6);
    expect(fatal).toHaveBeenCalledOnce();
    expect(api.clientDisconnect).toHaveBeenCalledOnce();
    expect(vi.getTimerCount()).toBe(0);
  });

  it('cancels pending reconnect and stats timers on close', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    Socket.instances[0].onclose?.();
    decoder.close();
    await vi.advanceTimersByTimeAsync(5000);
    expect(Socket.instances).toHaveLength(1);
    expect(vi.getTimerCount()).toBe(0);
  });

  it('sheds backlog by frame age, not only by queue depth', async () => {
    let now = 0;
    vi.spyOn(performance, 'now').mockImplementation(() => now);
    // 队列深度与延迟并不等价：8 帧在 30fps 是 267ms，在 60fps 是 133ms。
    // 队列不深但最新到达帧已比最新解码输出领先 100ms 以上时也应清理积压。
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const dec = Decoder.instances[0];
    const sock = Socket.instances[0];
    // 关键帧建立链路（pts=1000）
    sock.onmessage?.({ data: frameMsg(1, 1000) });
    await settleDecoder();
    expect(dec.decode).toHaveBeenCalledTimes(1);
    // 解码器吐出该帧 → 画面年龄归零
    dec.output({ timestamp: 1_000_000, close: () => {} });
    // 尚未输出的帧开始积压；此后媒体时间跨度超过上限。
    sock.onmessage?.({ data: frameMsg(0, 1033) });
    await settleDecoder();
    now = 150;
    sock.onmessage?.({ data: frameMsg(0, 1200) });
    await settleDecoder();
    expect(dec.decode).toHaveBeenCalledTimes(2); // 最后一帧没有送进解码器
    expect(api.clientRequestKeyframe).toHaveBeenCalled(); // 进入恢复门控
    expect(dec.close).toHaveBeenCalledOnce(); // 积压必须真正丢弃，不能继续解旧画面。
    const recovered = Decoder.instances.at(-1)!;
    expect(recovered).not.toBe(dec);
    sock.onmessage?.({ data: frameMsg(0, 1233) });
    await settleDecoder();
    expect(recovered.decode).not.toHaveBeenCalled();
    sock.onmessage?.({ data: frameMsg(1, 1300) });
    await settleDecoder();
    sock.onmessage?.({ data: frameMsg(0, 1333) });
    await settleDecoder();
    expect(recovered.decode).toHaveBeenCalledTimes(2);
    decoder.close();
  });

  it('accepts a media timestamp gap while pending work has only just arrived', async () => {
    let now = 0;
    vi.spyOn(performance, 'now').mockImplementation(() => now);
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const current = Decoder.instances[0];
    const socket = Socket.instances[0];
    socket.onmessage?.({ data: frameMsg(1, 1000) });
    await settleDecoder();
    now = 1;
    socket.onmessage?.({ data: frameMsg(0, 1250) });
    await settleDecoder();
    expect(current.decode).toHaveBeenCalledTimes(2);
    expect(current.close).not.toHaveBeenCalled();
    expect(api.clientRequestKeyframe).not.toHaveBeenCalled();
    decoder.close();
  });

  it('accepts a delta after an idle interval when no decoding work is outstanding', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const dec = Decoder.instances[0];
    const sock = Socket.instances[0];
    sock.onmessage?.({ data: frameMsg(1, 1000) });
    await settleDecoder();
    dec.output({ timestamp: 1_000_000, close: vi.fn() });
    sock.onmessage?.({ data: frameMsg(0, 5000) });
    await settleDecoder();
    expect(dec.decode).toHaveBeenCalledTimes(2);
    expect(api.clientRequestKeyframe).not.toHaveBeenCalled();
    decoder.close();
  });

  it('discards queued work when a fresh keyframe arrives under decoder backpressure', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const socket = Socket.instances[0];
    const old = Decoder.instances[0];
    socket.onmessage?.({ data: frameMsg(1, 1000) });
    await settleDecoder();
    old.decodeQueueSize = 9;
    socket.onmessage?.({ data: frameMsg(1, 1033) });
    await settleDecoder();
    const current = Decoder.instances.at(-1)!;
    expect(old.close).toHaveBeenCalledOnce();
    expect(old.decode).toHaveBeenCalledOnce();
    expect(current).not.toBe(old);
    expect(current.decode).toHaveBeenCalledOnce();
    socket.onmessage?.({ data: frameMsg(0, 1066) });
    await settleDecoder();
    expect(current.decode).toHaveBeenCalledTimes(2);
    expect(api.clientRequestKeyframe).not.toHaveBeenCalled();
    decoder.close();
  });

  it('allows startup delta frames while the first keyframe is still decoding', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const sock = Socket.instances[0];
    sock.onmessage?.({ data: frameMsg(1, 10_000) });
    await settleDecoder();
    sock.onmessage?.({ data: frameMsg(0, 10_033) });
    await settleDecoder();
    expect(Decoder.instances[0].decode).toHaveBeenCalledTimes(2);
    expect(api.clientRequestKeyframe).not.toHaveBeenCalled();
    decoder.close();
  });

  it('separates displayed fps from decoded fps', async () => {
    // 解码输出和上屏分别计数：displayed 只统计真正 drawImage 的帧，
    // 不包含绘制前被替换的解码帧。
    // 需要 canvas 才有绘图上下文，否则渲染循环不画任何帧
    const canvas = { width: 0, height: 0, getContext: () => ({ drawImage: vi.fn() }) };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const dec = Decoder.instances[0];
    const sock = Socket.instances[0];
    sock.onmessage?.({ data: frameMsg(1, 1000) });
    await settleDecoder();
    // 连吐两帧：第二帧会把第一帧挤掉（第一帧从未上屏）
    dec.output({ timestamp: 1_000_000, close: () => {} });
    dec.output({ timestamp: 1_033_000, close: () => {} });
    expect(decoder.stats.value.frames).toBe(0); // 统计按秒汇总，尚未刷新
    // 跑一次渲染循环：只画最新那一帧
    rafCallbacks.at(-1)?.();
    await vi.advanceTimersByTimeAsync(1000);
    expect(decoder.stats.value.fps).toBe(2); // 解码输出 2 帧
    expect(decoder.stats.value.displayed).toBe(1); // 实际只上屏 1 帧
    expect(decoder.stats.value.droppedDecoded).toBe(1);
    expect(decoder.stats.value.displayedFps).toBe(1);
    expect(decoder.stats.value.lagMs).toBe(0); // 输出已追平到达
    decoder.close();
  });

  it('waits for a keyframe and converts milliseconds to microseconds', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    const frame = new ArrayBuffer(10);
    const view = new DataView(frame);
    view.setUint32(1, 42);
    view.setUint32(5, 1);
    Socket.instances[0].onmessage?.({ data: frame });
    await settleDecoder();
    expect(Decoder.instances[0].decode).not.toHaveBeenCalled();
    view.setUint8(0, 1);
    Socket.instances[0].onmessage?.({ data: frame });
    await settleDecoder();
    expect(Decoder.instances[0].decode.mock.calls[0][0].init.timestamp).toBe(42_000);
    decoder.close();
  });

  it('retries keyframe recovery while rejecting empty or truncated video messages', async () => {
    const decoder = useDecoder(ref());
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const socket = Socket.instances[0];
    const empty = new ArrayBuffer(9);
    new DataView(empty).setUint8(0, 1);
    socket.onmessage?.({ data: empty });
    await settleDecoder();
    socket.onmessage?.({ data: frameMsg(1, 0).slice(0, 9) });
    await settleDecoder();
    await vi.advanceTimersByTimeAsync(2000);
    expect(Decoder.instances[0].decode).not.toHaveBeenCalled();
    expect(api.clientRequestKeyframe).toHaveBeenCalledTimes(2);
    socket.onmessage?.({ data: frameMsg(1, 1000) });
    await settleDecoder();
    Decoder.instances[0].output({ timestamp: 1_000_000, close: vi.fn() });
    await vi.advanceTimersByTimeAsync(1000);
    expect(api.clientRequestKeyframe).toHaveBeenCalledTimes(2);
    decoder.close();
  });

  it('measures latency from the displayed frame arrival, excluding later arrivals', async () => {
    let now = 0;
    vi.spyOn(performance, 'now').mockImplementation(() => now);
    const canvas = { width: 0, height: 0, getContext: () => ({ drawImage: vi.fn() }) };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    now = 100;
    Socket.instances[0].onmessage?.({ data: frameMsg(1, 1000) });
    await settleDecoder();
    now = 120;
    Socket.instances[0].onmessage?.({ data: frameMsg(0, 1033) });
    await settleDecoder();
    now = 140;
    Decoder.instances[0].output({ timestamp: 1_000_000, close: () => {} });
    now = 150;
    // A newer packet arrives while the previously decoded frame awaits rendering.
    Socket.instances[0].onmessage?.({ data: frameMsg(0, 1066) });
    await settleDecoder();
    now = 160;
    rafCallbacks.at(-1)?.();
    now = 1100;
    await vi.advanceTimersByTimeAsync(1000);
    expect(decoder.stats.value.p95Ms).toBe(60);
    decoder.close();
  });

  it('bounds keyframe backlog by media age and ignores outputs from retired decoders', async () => {
    let now = 0;
    vi.spyOn(performance, 'now').mockImplementation(() => now);
    const canvas = { width: 0, height: 0, getContext: () => ({ drawImage: vi.fn() }) };
    const decoder = useDecoder(ref(canvas as unknown as HTMLCanvasElement));
    decoder.connect(1234, 'token', initial);
    await settleDecoder();
    vi.mocked(api.clientRequestKeyframe).mockClear();
    const socket = Socket.instances[0];
    for (let pts = 1; pts <= 129; pts++) {
      now = pts;
      socket.onmessage?.({ data: frameMsg(1, pts) });
      await settleDecoder();
    }
    now = 200;
    const retired = { timestamp: 1000, close: vi.fn() };
    Decoder.instances[0].output(retired);
    rafCallbacks.at(-1)?.();
    now = 1100;
    await vi.advanceTimersByTimeAsync(1000);
    expect(Decoder.instances).toHaveLength(2);
    expect(retired.close).toHaveBeenCalledOnce();
    expect(decoder.stats.value.displayed).toBe(0);
    expect(decoder.stats.value.p95Ms).toBe(0);
    now = 1200;
    Decoder.instances.at(-1)!.output({ timestamp: 129_000, close: () => {} });
    rafCallbacks.at(-1)?.();
    now = 2200;
    await vi.advanceTimersByTimeAsync(1000);
    expect(decoder.stats.value.displayed).toBe(1);
    expect(decoder.stats.value.p95Ms).toBe(1071);
    decoder.close();
  });
});
