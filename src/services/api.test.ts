import { afterEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { api } from './api';
import { saveClipboardSyncPref, saveColorDepthPref } from './prefs';
afterEach(() => vi.unstubAllGlobals());

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));

describe('connection FEC snapshot', () => {
  it.each(['hybrid', 'tcp', 'quic'] as const)(
    'negotiates FEC only for %s UDP and preserves it through fallback',
    async (transport) => {
      const mockInvoke = vi.mocked(invoke);
      mockInvoke.mockReset();
      mockInvoke.mockResolvedValue({ sessionId: 42, targetWidth: 0, targetHeight: 0 });
      await api.clientConnect(
        '127.0.0.1',
        29875,
        'pw',
        ['hevc', 'h264'],
        30,
        8000,
        transport,
        true,
        null,
        'none',
        0,
        0,
        true,
      );
      expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
        params: expect.objectContaining({ fecEnabled: transport !== 'tcp' }),
      });
      await api.clientRetryH264(42);
      expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
        params: expect.objectContaining({ fecEnabled: transport !== 'tcp', codecs: ['h264'] }),
      });
    },
  );
});

describe('codec fallback connection', () => {
  it('passes hardware decoder capabilities through connection and H264 fallback', async () => {
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    mockInvoke.mockResolvedValue({ sessionId: 42, targetWidth: 0, targetHeight: 0 });
    await api.clientConnect(
      '127.0.0.1',
      29875,
      'pw',
      ['av1', 'h264'],
      30,
      8000,
      'tcp',
      false,
      null,
      'none',
      0,
      0,
      false,
      true,
      false,
      24,
      ['h264'],
    );
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({ codecs: ['av1', 'h264'], hardwareCodecs: ['h264'] }),
    });
    await api.clientRetryH264(42);
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({ codecs: ['h264'], hardwareCodecs: ['h264'] }),
    });
  });

  it('disconnects the failed session before renegotiating H264 with the same peer', async () => {
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockResolvedValueOnce({ sessionId: 42, targetWidth: 0, targetHeight: 0 });
    await api.clientConnect('192.168.1.20', 29875, 'pw', ['hevc', 'h264'], 60, 16000, 'tcp');
    mockInvoke.mockClear();
    mockInvoke.mockResolvedValueOnce(undefined).mockResolvedValueOnce({ sessionId: 43 });
    expect(await api.clientRetryH264(42)).toEqual({ sessionId: 43 });
    expect(mockInvoke.mock.calls).toEqual([
      ['client_disconnect', { sessionId: 42 }],
      [
        'client_connect',
        {
          params: {
            colorDepth: 24,
            showWallpaper: true,
            dragFullWindows: false,
            host: '192.168.1.20',
            port: 29875,
            password: 'pw',
            codecs: ['h264'],
            transport: 'tcp',
            fecEnabled: false,
            fps: 60,
            bitrateKbps: 16000,
            clipboardEnabled: true,
            audioEnabled: true,
            audioDeviceId: null,
            resolutionMode: 'none',
            targetWidth: 0,
            targetHeight: 0,
          },
        },
      ],
    ]);
  });
});

describe('per-connection transport', () => {
  it.each(['hybrid', 'tcp', 'quic'] as const)(
    'passes %s through window creation and connection retry',
    async (transport) => {
      const mockInvoke = vi.mocked(invoke);
      mockInvoke.mockReset();
      const params = {
        colorDepth: 24 as const,
        showWallpaper: true,
        dragFullWindows: false,
        resolutionMode: 'none' as const,
        targetWidth: 0,
        targetHeight: 0,
        audioDeviceId: null,
        clipboardEnabled: true,
        audioEnabled: true,
        transport,
        fecEnabled: false,
        host: '127.0.0.1',
        port: 29875,
        password: 'pw',
        codecs: ['h264'],
        fps: 60,
        bitrateKbps: 16000,
      };
      await api.openControlWindow(params);
      expect(mockInvoke).toHaveBeenLastCalledWith('open_control_window', { params });
      mockInvoke.mockResolvedValue(params);
      const restored = await api.controlWindowParams();
      await api.clientConnect(
        restored.host,
        restored.port,
        restored.password,
        restored.codecs,
        restored.fps,
        restored.bitrateKbps,
        restored.transport,
        restored.audioEnabled,
        restored.audioDeviceId,
      );
      expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', { params });
    },
  );
  it('defaults new connections to hybrid', async () => {
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    mockInvoke.mockResolvedValue({ targetWidth: 0, targetHeight: 0 });
    await api.clientConnect('127.0.0.1', 29875, 'pw', ['h264']);
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({ transport: 'hybrid' }),
    });
  });
});

describe('connection audio snapshot', () => {
  it('preserves disabled audio through codec fallback', async () => {
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    mockInvoke.mockResolvedValue({ sessionId: 42 });
    await api.clientConnect('127.0.0.1', 29875, 'pw', ['hevc', 'h264'], 30, 8000, 'hybrid', false);
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({ audioEnabled: false }),
    });
    await api.clientRetryH264(42);
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({ audioEnabled: false, codecs: ['h264'] }),
    });
  });
});

describe('selected audio endpoint', () => {
  it('lists endpoints through the native backend', async () => {
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    mockInvoke.mockResolvedValue([{ id: 'headset', name: '耳机', isDefault: true }]);
    expect(await api.listAudioPlaybackDevices()).toEqual([
      { id: 'headset', name: '耳机', isDefault: true },
    ]);
    expect(mockInvoke).toHaveBeenLastCalledWith('list_audio_playback_devices');
  });
  it('preserves the exact local device across window creation, connection and codec fallback', async () => {
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    const params = {
      colorDepth: 24 as const,
      showWallpaper: true,
      dragFullWindows: false,
      resolutionMode: 'none' as const,
      targetWidth: 0,
      targetHeight: 0,
      host: '127.0.0.1',
      port: 29875,
      password: 'pw',
      codecs: ['hevc', 'h264'],
      fps: 30,
      bitrateKbps: 8000,
      transport: 'tcp' as const,
      fecEnabled: false,
      clipboardEnabled: true,
      audioEnabled: true,
      audioDeviceId: 'headset-id',
    };
    await api.openControlWindow(params);
    expect(mockInvoke).toHaveBeenLastCalledWith('open_control_window', { params });
    mockInvoke.mockResolvedValue(params);
    const restored = await api.controlWindowParams();
    await api.clientConnect(
      restored.host,
      restored.port,
      restored.password,
      restored.codecs,
      restored.fps,
      restored.bitrateKbps,
      restored.transport,
      restored.audioEnabled,
      restored.audioDeviceId,
    );
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', { params });
    await api.clientRetryH264(42);
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: { ...params, codecs: ['h264'] },
    });
  });
});

describe('desktop effects snapshot', () => {
  it('keeps selected effects through window parameters and codec fallback', async () => {
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    const params = {
      colorDepth: 24 as const,
      host: '127.0.0.1',
      port: 29875,
      password: 'pw',
      codecs: ['hevc', 'h264'],
      fps: 30,
      bitrateKbps: 8000,
      transport: 'tcp' as const,
      clipboardEnabled: true,
      audioEnabled: false,
      audioDeviceId: null,
      resolutionMode: 'none' as const,
      targetWidth: 0,
      targetHeight: 0,
      fecEnabled: false,
      showWallpaper: false,
      dragFullWindows: true,
    };
    await api.openControlWindow(params);
    expect(mockInvoke).toHaveBeenLastCalledWith('open_control_window', { params });
    mockInvoke.mockResolvedValue(params);
    const restored = await api.controlWindowParams();
    mockInvoke.mockResolvedValue({ sessionId: 42, targetWidth: 0, targetHeight: 0 });
    await api.clientConnect(
      restored.host,
      restored.port,
      restored.password,
      restored.codecs,
      restored.fps,
      restored.bitrateKbps,
      restored.transport,
      restored.audioEnabled,
      restored.audioDeviceId,
      restored.resolutionMode,
      restored.targetWidth,
      restored.targetHeight,
      restored.fecEnabled,
      restored.showWallpaper,
      restored.dragFullWindows,
    );
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', { params });
    // Changing the home-window defaults cannot change this connection snapshot.
    await api.openControlWindow({ ...params, showWallpaper: true, dragFullWindows: false });
    await api.clientRetryH264(42);
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: { ...params, codecs: ['h264'] },
    });
  });
});

describe('resolution snapshot', () => {
  it('keeps the selected mode and physical target through H264 retry', async () => {
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    mockInvoke.mockResolvedValueOnce({ sessionId: 70, targetWidth: 2560, targetHeight: 1440 });
    await api.clientConnect(
      '127.0.0.1',
      29875,
      'pw',
      ['hevc', 'h264'],
      60,
      16000,
      'tcp',
      true,
      null,
      'change',
    );
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({
        resolutionMode: 'change',
        targetWidth: 0,
        targetHeight: 0,
      }),
    });
    mockInvoke.mockResolvedValue(undefined);
    await api.clientRetryH264(70);
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({
        resolutionMode: 'change',
        targetWidth: 2560,
        targetHeight: 1440,
        codecs: ['h264'],
      }),
    });
  });
});

describe('color depth snapshot', () => {
  it('snapshots the global preference when opening a window unless explicitly supplied', async () => {
    vi.stubGlobal('localStorage', { getItem: () => '16' });
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    const params = {
      host: '127.0.0.1',
      port: 29875,
      password: 'pw',
      codecs: ['h264'],
      fps: 30,
      bitrateKbps: 8000,
      transport: 'tcp' as const,
      clipboardEnabled: true,
      audioEnabled: false,
      audioDeviceId: null,
      resolutionMode: 'none' as const,
      targetWidth: 0,
      targetHeight: 0,
    };
    await api.openControlWindow(params);
    expect(mockInvoke).toHaveBeenLastCalledWith('open_control_window', {
      params: { ...params, colorDepth: 16 },
    });
    await api.openControlWindow({ ...params, colorDepth: 15 });
    expect(mockInvoke).toHaveBeenLastCalledWith('open_control_window', {
      params: { ...params, colorDepth: 15 },
    });
  });

  it.each([15, 16, 24, 32] as const)(
    'keeps %i bits through fallback after preferences change',
    async (depth) => {
      const store = new Map<string, string>();
      vi.stubGlobal('localStorage', {
        getItem: (key: string) => store.get(key) ?? null,
        setItem: (key: string, value: string) => void store.set(key, value),
      });
      const mockInvoke = vi.mocked(invoke);
      mockInvoke.mockReset();
      mockInvoke.mockResolvedValue({ sessionId: 42, targetWidth: 0, targetHeight: 0 });
      saveColorDepthPref(depth);
      await api.clientConnect('127.0.0.1', 29875, 'pw', ['hevc', 'h264']);
      expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
        params: expect.objectContaining({ colorDepth: depth }),
      });
      saveColorDepthPref(depth === 15 ? 32 : 15);
      await api.clientRetryH264(42);
      expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
        params: expect.objectContaining({ colorDepth: depth, codecs: ['h264'] }),
      });
    },
  );
});

describe('VNT node connection identity', () => {
  it('keeps the group, node and FEC through codec fallback', async () => {
    const call = vi.mocked(invoke);
    call.mockReset();
    call.mockResolvedValue({ sessionId: 43, targetWidth: 0, targetHeight: 0 });
    await api.clientConnect(
      '10.26.0.8',
      29875,
      '',
      ['hevc', 'h264'],
      30,
      8000,
      'vnt',
      false,
      null,
      'none',
      0,
      0,
      true,
      true,
      false,
      24,
      [],
      'node-8',
      'remote-group',
      'profile-office',
    );
    expect(call).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({
        transport: 'vnt',
        serverNodeId: 'node-8',
        serverNetworkCode: 'remote-group',
        serverConfigId: 'profile-office',
        password: '',
        fecEnabled: true,
      }),
    });
    await api.clientRetryH264(43);
    expect(call).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({
        transport: 'vnt',
        serverNodeId: 'node-8',
        serverNetworkCode: 'remote-group',
        serverConfigId: 'profile-office',
        fecEnabled: true,
        codecs: ['h264'],
      }),
    });
  });
});

it.each(['scale', 'change'] as const)(
  'preserves selected target through %s H264 retry',
  async (mode) => {
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    mockInvoke.mockResolvedValueOnce({ sessionId: 80, targetWidth: 1280, targetHeight: 720 });
    await api.clientConnect(
      '127.0.0.1',
      29875,
      'pw',
      ['hevc', 'h264'],
      30,
      16000,
      'tcp',
      false,
      null,
      mode,
      1280,
      720,
    );
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({
        resolutionMode: mode,
        targetWidth: 1280,
        targetHeight: 720,
      }),
    });
    mockInvoke.mockResolvedValue(undefined);
    await api.clientRetryH264(80);
    expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
      params: expect.objectContaining({
        resolutionMode: mode,
        targetWidth: 1280,
        targetHeight: 720,
        codecs: ['h264'],
      }),
    });
  },
);

describe('clipboard preference at connection start', () => {
  it.each([false, true])(
    'passes the %s preference before connect and preserves it on fallback',
    async (enabled) => {
      const store = new Map<string, string>();
      vi.stubGlobal('localStorage', {
        getItem: (key: string) => store.get(key) ?? null,
        setItem: (key: string, value: string) => void store.set(key, value),
      });
      saveClipboardSyncPref(enabled);
      const mockInvoke = vi.mocked(invoke);
      mockInvoke.mockReset();
      mockInvoke.mockResolvedValue({ sessionId: 42, targetWidth: 0, targetHeight: 0 });
      await api.clientConnect('127.0.0.1', 29875, 'pw', ['hevc', 'h264']);
      expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
        params: expect.objectContaining({ clipboardEnabled: enabled }),
      });
      saveClipboardSyncPref(!enabled);
      await api.clientRetryH264(42);
      expect(mockInvoke).toHaveBeenLastCalledWith('client_connect', {
        params: expect.objectContaining({ clipboardEnabled: enabled, codecs: ['h264'] }),
      });
    },
  );

  it('snapshots the preference when opening a window and honors an explicit value', async () => {
    vi.stubGlobal('localStorage', { getItem: () => '0' });
    const mockInvoke = vi.mocked(invoke);
    mockInvoke.mockReset();
    const params = {
      host: '127.0.0.1',
      port: 29875,
      password: 'pw',
      codecs: ['h264'],
      fps: 30,
      bitrateKbps: 8000,
      transport: 'tcp' as const,
      audioEnabled: false,
      audioDeviceId: null,
      resolutionMode: 'none' as const,
      targetWidth: 0,
      targetHeight: 0,
    };
    await api.openControlWindow(params);
    expect(mockInvoke).toHaveBeenLastCalledWith('open_control_window', {
      params: expect.objectContaining({ clipboardEnabled: false }),
    });
    await api.openControlWindow({ ...params, clipboardEnabled: true });
    expect(mockInvoke).toHaveBeenLastCalledWith('open_control_window', {
      params: expect.objectContaining({ clipboardEnabled: true }),
    });
  });
});
