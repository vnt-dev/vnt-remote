import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ref } from 'vue';
import { useRemoteDecoder } from './useRemoteDecoder';
import { api, type ClientSessionInfo, type RemoteScreen } from '../services/api';

vi.mock('../services/api', () => ({
  api: {
    clientRequestKeyframe: vi.fn().mockResolvedValue(undefined),
    clientDisconnect: vi.fn().mockResolvedValue(undefined),
    clientScreenCommand: vi.fn().mockResolvedValue(undefined),
    detachScreen: vi.fn().mockResolvedValue('remote-screen-1'),
  },
}));
class Socket {
  static instances: Socket[] = [];
  onmessage: ((event: { data: string | ArrayBuffer }) => void) | null = null;
  onclose: (() => void) | null = null;
  onopen: (() => void) | null = null;
  binaryType = '';
  constructor() {
    Socket.instances.push(this);
  }
  close = vi.fn();
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
  output: (frame: { timestamp: number; close: () => void }) => void;
  error: (error: DOMException) => void;
  constructor(init: { output: Decoder['output']; error: Decoder['error'] }) {
    this.output = init.output;
    this.error = init.error;
    Decoder.instances.push(this);
  }
}
const screens: RemoteScreen[] = [1, 2].map((streamId) => ({
  streamId,
  name: `屏幕${streamId}`,
  width: 64 * streamId,
  height: 48,
  fps: 30,
  codec: 'h264',
  hardware: false,
  available: true,
  reason: '',
  generation: 1,
}));
const session = { sessionId: 42, transport: 'tcp', screens } as ClientSessionInfo;
let callbacks: Array<() => void>;
function paint() {
  const batch = callbacks;
  callbacks = [];
  batch.forEach((callback) => callback());
}
function packet(id: number, generation = 1) {
  const data = new ArrayBuffer(18);
  const view = new DataView(data);
  view.setUint32(0, id);
  view.setUint32(4, generation);
  view.setUint8(8, 1);
  view.setUint32(9, 1);
  view.setUint32(13, 1);
  return data;
}
function state(
  active: number,
  revision: number,
  selected = screens,
  detachedScreens: number[] = [],
) {
  Socket.instances[0].onmessage?.({
    data: JSON.stringify({
      type: 'screensState',
      activeStreamId: active,
      revision,
      screens: selected,
      detachedScreens,
    }),
  });
}

describe('simultaneous remote screen decoding', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
    callbacks = [];
    Socket.instances = [];
    Decoder.instances = [];
    Decoder.isConfigSupported.mockResolvedValue({ supported: true });
    vi.stubGlobal('WebSocket', Socket);
    vi.stubGlobal('VideoDecoder', Decoder);
    vi.stubGlobal(
      'EncodedVideoChunk',
      class {
        constructor(public value: unknown) {}
      },
    );
    vi.stubGlobal('requestAnimationFrame', (callback: () => void) => {
      callbacks.push(callback);
      return callbacks.length;
    });
    vi.stubGlobal('cancelAnimationFrame', vi.fn());
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('decodes hidden screens through one socket and switches using retained frames', async () => {
    const drawImage = vi.fn();
    const canvas = { width: 64, height: 48, getContext: () => ({ drawImage, clearRect: vi.fn() }) };
    const remote = useRemoteDecoder(ref(canvas as unknown as HTMLCanvasElement), session, vi.fn());
    remote.connect(1234, 'token', screens[0]);
    await settleDecoder();
    state(1, 1);
    await settleDecoder();
    expect(Socket.instances).toHaveLength(1);
    expect(Decoder.instances).toHaveLength(2);
    const socket = Socket.instances[0];
    socket.onmessage?.({ data: packet(1) });
    await settleDecoder();
    socket.onmessage?.({ data: packet(2) });
    await settleDecoder();
    expect(Decoder.instances[0].decode).toHaveBeenCalledOnce();
    expect(Decoder.instances[1].decode).toHaveBeenCalledOnce();
    const first = { timestamp: 1000, close: vi.fn() },
      second = { timestamp: 1000, close: vi.fn() };
    Decoder.instances[0].output(first);
    Decoder.instances[1].output(second);
    paint();
    expect(drawImage).toHaveBeenLastCalledWith(first, 0, 0, 64, 48);
    const requests = vi.mocked(api.clientRequestKeyframe).mock.calls.length;
    await remote.select(2);
    paint();
    expect(drawImage).toHaveBeenLastCalledWith(second, 0, 0, 128, 48);
    state(2, 2);
    await settleDecoder();
    await remote.select(1);
    paint();
    state(1, 3);
    await settleDecoder();
    expect(drawImage).toHaveBeenLastCalledWith(first, 0, 0, 64, 48);
    expect(Decoder.instances).toHaveLength(2);
    expect(api.clientRequestKeyframe).toHaveBeenCalledTimes(requests);
    expect(first.close).not.toHaveBeenCalled();
    expect(second.close).not.toHaveBeenCalled();
    remote.close();
    expect(first.close).toHaveBeenCalledOnce();
    expect(second.close).toHaveBeenCalledOnce();
  });

  it('disposes a closed route, ignores late packets and refuses to close the last screen', async () => {
    const canvas = {
      width: 64,
      height: 48,
      getContext: () => ({ drawImage: vi.fn(), clearRect: vi.fn() }),
    };
    const remote = useRemoteDecoder(ref(canvas as unknown as HTMLCanvasElement), session, vi.fn());
    remote.connect(1234, 'token', screens[0]);
    await settleDecoder();
    state(1, 1);
    await settleDecoder();
    await remote.closeScreen(1);
    expect(api.clientScreenCommand).toHaveBeenLastCalledWith(42, 1, 1, true);
    state(2, 2, [screens[1]]);
    await settleDecoder();
    expect(Decoder.instances[0].close).toHaveBeenCalledOnce();
    Socket.instances[0].onmessage?.({ data: packet(1) });
    await settleDecoder();
    expect(Decoder.instances[0].decode).not.toHaveBeenCalled();
    await remote.closeScreen(2);
    expect(api.clientScreenCommand).toHaveBeenCalledTimes(1);
    state(1, 1);
    await settleDecoder();
    expect(remote.screens.value.map((screen) => screen.streamId)).toEqual([2]);
    remote.close();
  });

  it('keeps unrelated decoders when one screen changes generation', async () => {
    const canvas = {
      width: 64,
      height: 48,
      getContext: () => ({ drawImage: vi.fn(), clearRect: vi.fn() }),
    };
    const remote = useRemoteDecoder(ref(canvas as unknown as HTMLCanvasElement), session, vi.fn());
    remote.connect(1234, 'token', screens[0]);
    await settleDecoder();
    state(1, 1);
    await settleDecoder();
    const other = Decoder.instances[1];
    Socket.instances[0].onmessage?.({
      data: JSON.stringify({ type: 'streamInfo', ...screens[0], generation: 2 }),
    });
    await settleDecoder();
    expect(other.close).not.toHaveBeenCalled();
    Socket.instances[0].onmessage?.({ data: packet(1, 1) });
    await settleDecoder();
    expect(Decoder.instances.at(-1)?.decode).not.toHaveBeenCalled();
    Socket.instances[0].onmessage?.({ data: packet(2, 1) });
    await settleDecoder();
    expect(other.decode).toHaveBeenCalledOnce();
    remote.close();
  });
  function createView(info: ClientSessionInfo = session) {
    const canvas = {
      width: 64,
      height: 48,
      getContext: () => ({ drawImage: vi.fn(), clearRect: vi.fn() }),
    };
    const remote = useRemoteDecoder(ref(canvas as unknown as HTMLCanvasElement), info, vi.fn());
    remote.connect(1234, 'token', screens[0]);
    return remote;
  }

  it('releases the detached decoder and keeps the owner view on its remaining screen', async () => {
    const remote = createView();
    await settleDecoder();
    state(2, 1);
    await settleDecoder();
    expect(remote.activeStreamId.value).toBe(2);
    const detached = Decoder.instances[1];
    await remote.detach(2);
    expect(api.detachScreen).toHaveBeenCalledExactlyOnceWith(42, 2);
    state(2, 1, screens, [2]);
    await settleDecoder();
    expect(remote.activeStreamId.value).toBe(1);
    expect(remote.localScreens.value.map((s) => s.streamId)).toEqual([1]);
    expect(detached.close).toHaveBeenCalledOnce();
    Socket.instances[0].onmessage?.({ data: packet(2) });
    await settleDecoder();
    expect(detached.decode).not.toHaveBeenCalled();
    state(2, 2, screens, [2]);
    await settleDecoder();
    expect(remote.activeStreamId.value).toBe(1);
    await remote.detach(1);
    await remote.closeScreen(1);
    expect(api.detachScreen).toHaveBeenCalledTimes(1);
    expect(api.clientScreenCommand).not.toHaveBeenCalled();
    await remote.select(2);
    expect(api.detachScreen).toHaveBeenCalledTimes(2);
    remote.close();
  });

  it('pins a child view while another screen owns remote input', async () => {
    const remote = createView({ ...session, screens: [screens[1]], windowStreamId: 2 });
    await settleDecoder();
    state(1, 1, [screens[1]], [2]);
    await settleDecoder();
    expect(remote.activeStreamId.value).toBe(2);
    expect(remote.inputTarget.value).toEqual({ streamId: 2, generation: 1, revision: 1 });
    expect(Decoder.instances).toHaveLength(1);
    Socket.instances[0].onmessage?.({ data: packet(2) });
    await settleDecoder();
    expect(Decoder.instances[0].decode).toHaveBeenCalledOnce();
    state(1, 2, [screens[1]], [2]);
    await settleDecoder();
    await remote.select(1);
    await remote.detach(2);
    await remote.closeScreen(2);
    expect(api.detachScreen).not.toHaveBeenCalled();
    expect(api.clientScreenCommand).not.toHaveBeenCalled();
    expect(remote.activeStreamId.value).toBe(2);
    remote.close();
    expect(api.clientDisconnect).not.toHaveBeenCalled();
  });

  it('leaves ownership intact after window creation fails and permits retry', async () => {
    const remote = createView();
    await settleDecoder();
    state(1, 1);
    await settleDecoder();
    vi.mocked(api.detachScreen).mockRejectedValueOnce(new Error('window failed'));
    await remote.detach(2);
    expect(remote.commandError.value).toContain('window failed');
    expect(remote.busy.value).toBe(false);
    expect(remote.localScreens.value).toHaveLength(2);
    expect(Decoder.instances[1].close).not.toHaveBeenCalled();
    await remote.detach(2);
    expect(api.detachScreen).toHaveBeenCalledTimes(2);
    expect(remote.commandError.value).toBe('');
    remote.close();
  });

  it('keeps a closed child excluded while its host closure is pending', async () => {
    const remote = createView();
    await settleDecoder();
    state(2, 1, screens, [2]);
    await settleDecoder();
    state(2, 2, screens, [2]);
    await settleDecoder();
    expect(remote.localScreens.value.map((s) => s.streamId)).toEqual([1]);
    state(1, 3, [screens[0]], [2]);
    await settleDecoder();
    expect(remote.screens.value).toHaveLength(1);
    expect(remote.activeStreamId.value).toBe(1);
    expect(Decoder.instances).toHaveLength(2);
    remote.close();
  });

  it('allows multiple children while preserving the owner selection on bridge recovery', async () => {
    const third = { ...screens[1], streamId: 3, name: '屏幕3' };
    const all = [...screens, third];
    const remote = createView({ ...session, screens: all });
    await settleDecoder();
    state(1, 1, all);
    await settleDecoder();
    await remote.detach(2);
    state(1, 1, all, [2]);
    await settleDecoder();
    await remote.detach(3);
    state(3, 2, all, [2, 3]);
    await settleDecoder();
    expect(remote.localScreens.value.map((s) => s.streamId)).toEqual([1]);
    Socket.instances[0].onopen?.();
    await settleDecoder();
    state(3, 2, all, [2, 3]);
    await settleDecoder();
    expect(remote.activeStreamId.value).toBe(1);
    // Reopening the bridge replaces only the remaining screen's native decoder.
    expect(Decoder.instances).toHaveLength(4);
    expect(Decoder.instances[1].close).toHaveBeenCalledOnce();
    expect(Decoder.instances[2].close).toHaveBeenCalledOnce();
    remote.close();
  });
  it('never displays a detached screen when a parent selection times out during a control handoff', async () => {
    const all = [...screens, { ...screens[1], streamId: 3 }];
    const remote = createView({ ...session, screens: all });
    await settleDecoder();
    state(1, 1, all, [3]);
    await settleDecoder();
    await remote.select(2);
    state(3, 2, all, [3]);
    await settleDecoder();
    await vi.advanceTimersByTimeAsync(5000);
    expect(remote.activeStreamId.value).toBe(2);
    expect(remote.busy.value).toBe(false);
    expect(remote.commandError.value).toContain('尚未确认');
    remote.close();
  });
});
