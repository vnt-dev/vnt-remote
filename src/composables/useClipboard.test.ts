import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { useClipboard } from './useClipboard';
import type { ClipboardTransfer } from '../services/api';
const mocks = vi.hoisted(() => ({
  handler: undefined as undefined | ((event: { payload: ClipboardTransfer }) => void),
  cancel: vi.fn(),
  enabled: vi.fn(() => Promise.resolve()),
  current: vi.fn().mockResolvedValue(false),
}));
vi.mock('vue', async (original) => ({
  ...(await original<typeof import('vue')>()),
  onUnmounted: vi.fn(),
}));
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(async (_name, handler) => {
    mocks.handler = handler;
    return vi.fn();
  }),
}));
vi.mock('../services/api', () => ({
  api: {
    clipboardCancelTransfer: mocks.cancel,
    clientSetClipboardEnabled: mocks.enabled,
    clientClipboardEnabled: mocks.current,
  },
}));
const event = (change: Partial<ClipboardTransfer> = {}): ClipboardTransfer => ({
  connectionId: '17',
  sessionId: 42,
  transferId: 3,
  direction: 'receive',
  state: 'active',
  name: 'a.bin',
  fileIndex: 0,
  fileCount: 1,
  completedBytes: 2,
  totalBytes: 9,
  speedKbps: 1,
  ...change,
});
describe('native clipboard transfer panel', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.clearAllMocks();
  });
  afterEach(() => {
    vi.useRealTimers();
  });
  it('isolates connections even when remote session numbers match', async () => {
    const clipboard = useClipboard(42, '17');
    await clipboard.subscribe();
    mocks.handler!({ payload: event({ connectionId: '18' }) });
    expect(clipboard.transfers.value).toEqual([]);
    mocks.handler!({ payload: event() });
    mocks.handler!({ payload: event({ completedBytes: 8 }) });
    expect(clipboard.transfers.value).toHaveLength(1);
    expect(clipboard.transfers.value[0].completedBytes).toBe(8);
    mocks.handler!({ payload: event({ state: 'done', completedBytes: 9 }) });
    vi.advanceTimersByTime(5000);
    expect(clipboard.transfers.value).toEqual([]);
  });
  it('routes cancellation and the switch by the local connection key', async () => {
    const clipboard = useClipboard(42, '17');
    await clipboard.cancel(3);
    await clipboard.setEnabled(false);
    expect(mocks.cancel).toHaveBeenCalledWith('17', 3);
    expect(mocks.enabled).toHaveBeenCalledWith('17', false);
    expect(clipboard.enabled.value).toBe(false);
  });

  it('does not overwrite the registered session state when the UI subscribes', async () => {
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => (key === 'vnt.clipboardSync' ? '0' : null),
      setItem: () => undefined,
      removeItem: () => undefined,
    });
    const clipboard = useClipboard(42, '17', true);
    expect(clipboard.enabled.value).toBe(true);
    await clipboard.subscribe();
    expect(clipboard.enabled.value).toBe(true);
    expect(mocks.enabled).not.toHaveBeenCalled();
    vi.unstubAllGlobals();
  });
  it('does not reapply a local preference when a child joins an existing session', async () => {
    const clipboard = useClipboard(42, '17', false, true);
    await clipboard.subscribe();
    expect(clipboard.enabled.value).toBe(false);
    expect(mocks.enabled).not.toHaveBeenCalled();
  });
});
