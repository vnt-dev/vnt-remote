import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  loadTargetResolutionPref,
  saveTargetResolutionPref,
  loadColorDepthPref,
  saveColorDepthPref,
  loadShowWallpaperPref,
  saveShowWallpaperPref,
  loadDragFullWindowsPref,
  saveDragFullWindowsPref,
  loadResolutionModePref,
  saveResolutionModePref,
  loadClipboardSyncPref,
  saveClipboardSyncPref,
  loadAudioPlaybackPref,
  saveAudioPlaybackPref,
} from './prefs';

afterEach(() => vi.unstubAllGlobals());

describe('new connection desktop effects', () => {
  it('defaults to wallpaper on and full-window drag off, and persists independently', () => {
    const store = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => void store.set(key, value),
    });
    expect(loadShowWallpaperPref()).toBe(true);
    expect(loadDragFullWindowsPref()).toBe(false);
    saveShowWallpaperPref(false);
    saveDragFullWindowsPref(true);
    expect(loadShowWallpaperPref()).toBe(false);
    expect(loadDragFullWindowsPref()).toBe(true);
    saveShowWallpaperPref(true);
    saveDragFullWindowsPref(false);
    expect(loadShowWallpaperPref()).toBe(true);
    expect(loadDragFullWindowsPref()).toBe(false);
  });
  it('uses defaults when storage is inaccessible or invalid', () => {
    vi.stubGlobal('localStorage', { getItem: () => 'invalid' });
    expect(loadShowWallpaperPref()).toBe(true);
    expect(loadDragFullWindowsPref()).toBe(false);
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('unavailable');
      },
      setItem: () => {
        throw new Error('unavailable');
      },
    });
    expect(loadShowWallpaperPref()).toBe(true);
    expect(loadDragFullWindowsPref()).toBe(false);
    expect(() => saveShowWallpaperPref(false)).not.toThrow();
    expect(() => saveDragFullWindowsPref(true)).not.toThrow();
  });
});

describe('clipboard sync preference', () => {
  it('defaults to on and round-trips through storage', () => {
    const store = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => void store.set(key, value),
      removeItem: (key: string) => void store.delete(key),
    });
    expect(loadClipboardSyncPref()).toBe(true);
    saveClipboardSyncPref(false);
    expect(loadClipboardSyncPref()).toBe(false);
    saveClipboardSyncPref(true);
    expect(loadClipboardSyncPref()).toBe(true);
  });
});

describe('new connection audio preference', () => {
  it('defaults to system selection, persists endpoints and opt-out, and tolerates unavailable storage', () => {
    const store = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => void store.set(key, value),
    });
    expect(loadAudioPlaybackPref()).toBeNull();
    saveAudioPlaybackPref('off');
    expect(loadAudioPlaybackPref()).toBe('off');
    expect(loadClipboardSyncPref()).toBe(true);
    saveAudioPlaybackPref('headset-id');
    expect(loadAudioPlaybackPref()).toBe('headset-id');
    saveAudioPlaybackPref(null);
    expect(loadAudioPlaybackPref()).toBeNull();
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('unavailable');
      },
      setItem: () => {
        throw new Error('unavailable');
      },
    });
    expect(loadAudioPlaybackPref()).toBeNull();
    expect(() => saveAudioPlaybackPref('off')).not.toThrow();
  });
});

describe('resolution preference', () => {
  it('defaults to none and persists each mode', () => {
    const store = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => void store.set(key, value),
    });
    expect(loadResolutionModePref()).toBe('none');
    for (const mode of ['scale', 'change', 'none'] as const) {
      saveResolutionModePref(mode);
      expect(loadResolutionModePref()).toBe(mode);
    }
  });
});

describe('new connection color depth', () => {
  it('persists all four depths and defaults missing or invalid values to 24', () => {
    const store = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => void store.set(key, value),
    });
    expect(loadColorDepthPref()).toBe(24);
    for (const depth of [15, 16, 24, 32] as const) {
      saveColorDepthPref(depth);
      expect(loadColorDepthPref()).toBe(depth);
    }
    for (const invalid of ['0', '8', '30', 'invalid', '16.0', '']) {
      store.set('vnt.colorDepth', invalid);
      expect(loadColorDepthPref()).toBe(24);
    }
  });
  it('uses 24 when storage is unavailable', () => {
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('unavailable');
      },
      setItem: () => {
        throw new Error('unavailable');
      },
    });
    expect(loadColorDepthPref()).toBe(24);
    expect(() => saveColorDepthPref(15)).not.toThrow();
  });
});

it('defaults target size to the screen and persists a lower resolution independently', () => {
  const store = new Map<string, string>();
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => store.set(key, value),
  });
  expect(loadTargetResolutionPref()).toBe('screen');
  saveTargetResolutionPref('1280x720');
  saveResolutionModePref('none');
  expect(loadTargetResolutionPref()).toBe('1280x720');
  store.set('vnt.targetResolution', 'invalid');
  expect(loadTargetResolutionPref()).toBe('screen');
});
