import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  decoderConfig,
  loadHwDecodePref,
  saveHwDecodePref,
  selectDecoderConfig,
  supportedCodecList,
  supportedHardwareCodecList,
} from './codec';

afterEach(() => vi.unstubAllGlobals());

describe('codec negotiation', () => {
  it('advertises only hardware-supported codecs using the playback configuration', async () => {
    vi.stubGlobal('localStorage', { getItem: () => '1' });
    const isConfigSupported = vi.fn(async (config: VideoDecoderConfig) => {
      if (config.codec.startsWith('av01')) throw new Error('driver unavailable');
      return { supported: config.codec.startsWith('hev1') };
    });
    vi.stubGlobal('VideoDecoder', { isConfigSupported });
    expect(await supportedHardwareCodecList(['av1', 'hevc', 'h264'])).toEqual(['hevc']);
    expect(isConfigSupported).toHaveBeenCalledWith(
      decoderConfig('hevc', 3840, 2160, 'prefer-hardware'),
    );
    expect(isConfigSupported).toHaveBeenCalledTimes(3);
  });

  it('does not advertise hardware when hardware decoding is disabled or unavailable', async () => {
    const isConfigSupported = vi.fn(async () => ({ supported: true }));
    vi.stubGlobal('VideoDecoder', { isConfigSupported });
    vi.stubGlobal('localStorage', { getItem: () => '0' });
    expect(await supportedHardwareCodecList(['av1', 'hevc', 'h264'])).toEqual([]);
    expect(isConfigSupported).not.toHaveBeenCalled();
    vi.stubGlobal('localStorage', { getItem: () => '1' });
    vi.stubGlobal('VideoDecoder', undefined);
    expect(await supportedHardwareCodecList(['h264'])).toEqual([]);
  });

  it('only advertises codecs supported at the same 4K configuration used by playback', async () => {
    const isConfigSupported = vi.fn(async (config: VideoDecoderConfig) => ({
      supported: config.codec.startsWith('avc1'),
    }));
    vi.stubGlobal('VideoDecoder', { isConfigSupported });
    expect(await supportedCodecList()).toEqual(['h264']);
    expect(isConfigSupported).toHaveBeenCalledWith(decoderConfig('hevc', 3840, 2160));
    expect(isConfigSupported).toHaveBeenCalledWith(decoderConfig('h264', 3840, 2160));
    expect(decoderConfig('hevc', 3840, 2160).hardwareAcceleration).toBe('no-preference');
  });
});

describe('hardware decode preference', () => {
  it('defaults to on and round-trips through storage', () => {
    const store = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => void store.set(key, value),
      removeItem: (key: string) => void store.delete(key),
    });
    expect(loadHwDecodePref()).toBe(true);
    saveHwDecodePref(false);
    expect(loadHwDecodePref()).toBe(false);
    saveHwDecodePref(true);
    expect(loadHwDecodePref()).toBe(true);
  });
});

describe('playback configuration support', () => {
  it('keeps supported hardware settings and probes the actual stream dimensions', async () => {
    const isConfigSupported = vi.fn(async () => ({ supported: true }));
    vi.stubGlobal('VideoDecoder', { isConfigSupported });
    const config = decoderConfig('hevc', 2560, 1440, 'prefer-hardware');
    expect(await selectDecoderConfig(config)).toEqual(config);
    expect(isConfigSupported).toHaveBeenCalledExactlyOnceWith(config);
  });

  it('tries software when the hardware capability check throws', async () => {
    const isConfigSupported = vi
      .fn()
      .mockRejectedValueOnce(new Error('driver unavailable'))
      .mockResolvedValueOnce({ supported: true });
    vi.stubGlobal('VideoDecoder', { isConfigSupported });
    const config = decoderConfig('av1', 3840, 2160, 'prefer-hardware');
    expect(await selectDecoderConfig(config)).toEqual({
      ...config,
      hardwareAcceleration: 'prefer-software',
    });
  });

  it('honors an explicit software preference without probing hardware', async () => {
    const isConfigSupported = vi.fn(async () => ({ supported: true }));
    vi.stubGlobal('VideoDecoder', { isConfigSupported });
    const config = decoderConfig('h264', 1920, 1080, 'prefer-software');
    expect(await selectDecoderConfig(config)).toEqual(config);
    expect(isConfigSupported).toHaveBeenCalledExactlyOnceWith(config);
  });
});
