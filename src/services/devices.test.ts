import { describe, expect, it } from 'vitest';
import { parseDevices, writeDevices, type SavedDevice } from './devices';
const device = {
  id: 'a',
  name: 'PC',
  addr: '127.0.0.1:29875',
  password: 'pw',
  transport: 'tcp',
  fecEnabled: false,
  protocol: 'auto',
  fps: 30,
  bitrateKbps: 8000,
  lastUsed: 0,
  colorDepth: 24,
  resolutionMode: 'none',
  showWallpaper: true,
  dragFullWindows: false,
  hwDecode: true,
  clipboardSync: true,
  audioPlaybackDevice: null,
};
const encode = (d: object) => JSON.stringify({ formatVersion: 1, devices: [d] });
describe('device storage', () => {
  it('reads complete records and preserves the exact saved preferences', () => {
    expect(parseDevices(null)).toEqual([]);
    expect(parseDevices(encode(device))).toEqual([device]);
  });
  it('rejects malformed records and missing preferences', () => {
    for (const raw of ['{}', '{', JSON.stringify({ formatVersion: 2, devices: [] })])
      expect(() => parseDevices(raw)).toThrow();
    for (const key of ['colorDepth', 'fecEnabled', 'showWallpaper', 'audioPlaybackDevice']) {
      const incomplete: Record<string, unknown> = { ...device };
      delete incomplete[key];
      expect(() => parseDevices(encode(incomplete))).toThrow();
    }
    expect(() => parseDevices(encode({ ...device, fps: 120 }))).toThrow();
  });
});

it('writes the complete device-list envelope', () => {
  let raw: string | null = null;
  const storage = {
    setItem: (_key: string, value: string) => {
      raw = value;
    },
  };
  writeDevices(storage, [device as SavedDevice]);
  expect(JSON.parse(raw!)).toEqual({ formatVersion: 1, devices: [device] });
  expect(parseDevices(raw)).toEqual([device]);
});

it('accepts old device records and preserves device-specific target sizes', () => {
  expect(parseDevices(encode(device))[0]!.targetResolution).toBeUndefined();
  expect(
    parseDevices(encode({ ...device, targetResolution: '1280x720' }))[0]!.targetResolution,
  ).toBe('1280x720');
  expect(() => parseDevices(encode({ ...device, targetResolution: 'broken' }))).toThrow();
});

it.each(['192.168.1.10', '192.168.1.10:31000', 'example.com', 'example.com:31000', '[::1]:31000'])(
  'preserves supported addresses on save and reload: %s',
  (addr) => {
    const saved = { ...device, addr } as SavedDevice;
    let raw: string | null = null;
    writeDevices(
      {
        setItem: (_key, value) => {
          raw = value;
        },
      },
      [saved],
    );
    expect(parseDevices(raw)).toEqual([saved]);
  },
);
it.each(['192.168.1.10:', 'example.com:', 'host:65536', 'https://example.com'])(
  'rejects invalid device addresses: %s',
  (addr) => {
    expect(() => parseDevices(encode({ ...device, addr }))).toThrow('设备列表格式无效');
  },
);
it('preserves overlay connection IDs', () => {
  const saved = { ...device, addr: '123456', transport: 'vnt', serverNodeId: 'node-1' };
  expect(parseDevices(encode(saved))).toEqual([saved]);
});
