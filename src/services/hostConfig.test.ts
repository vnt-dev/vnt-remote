import { describe, expect, it, vi } from 'vitest';
import type { AppConfig } from './api';
import { createHostConfigSaver } from './hostConfig';

function config(): AppConfig {
  return {
    password: 'old-password',
    passwordConfigured: true,
    servers: [],
    port: 29875,
    formatVersion: 1,
    selectedTargets: [],
    screenRegions: [],
    clipboard: true,
    audio: true,
    hardwareEncode: true,
    allowControl: true,
    captureBorder: false,
  };
}
function gate() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

describe('automatic host configuration saves', () => {
  it('deep snapshots regions and preserves newer array edits during a pending save', async () => {
    const draft = config();
    const region = {
      id: 'r',
      name: 'first',
      monitorName: 'screen',
      screenWidth: 1920,
      screenHeight: 1080,
      orientation: 0,
      x: 0,
      y: 0,
      width: 640,
      height: 480,
    };
    draft.screenRegions = [region];
    const firstWrite = gate();
    let stored = config();
    const setConfig = vi.fn(async (value: AppConfig) => {
      if (setConfig.mock.calls.length === 1) await firstWrite.promise;
      stored = JSON.parse(JSON.stringify(value));
    });
    const save = createHostConfigSaver(draft, { setConfig, getConfig: async () => stored });
    const first = save();
    region.name = 'second';
    const second = save();
    await vi.waitFor(() => expect(setConfig).toHaveBeenCalledOnce());
    firstWrite.resolve();
    await first;
    expect(draft.screenRegions[0]?.name).toBe('second');
    await second;
    expect(setConfig.mock.calls.map(([value]) => value.screenRegions[0]?.name)).toEqual([
      'first',
      'second',
    ]);
    expect(stored.screenRegions[0]?.name).toBe('second');
  });
  it('waits for the pending password save before copying without writing again', async () => {
    const draft = config();
    const writing = gate();
    const setConfig = vi.fn(() => writing.promise);
    const save = createHostConfigSaver(draft, {
      setConfig,
      getConfig: async () => ({ ...draft }),
    });
    const saving = save();
    let flushed = false;
    const flushing = save.flush().then(() => {
      flushed = true;
    });
    await vi.waitFor(() => expect(setConfig).toHaveBeenCalledOnce());
    expect(flushed).toBe(false);
    writing.resolve();
    await Promise.all([saving, flushing]);
    expect(flushed).toBe(true);
    expect(setConfig).toHaveBeenCalledOnce();
  });

  it('does not allow copying after the latest save failed', async () => {
    const save = createHostConfigSaver(config(), {
      setConfig: async () => {
        throw new Error('配置文件不可写');
      },
      getConfig: async () => config(),
    });
    await expect(save()).rejects.toThrow('配置文件不可写');
    await expect(save.flush()).rejects.toThrow('配置文件不可写');
  });

  it('serializes edits and preserves a newer password while an earlier save finishes', async () => {
    const draft = config();
    const firstWrite = gate();
    let stored = config();
    const setConfig = vi.fn(async (value: AppConfig) => {
      if (setConfig.mock.calls.length === 1) await firstWrite.promise;
      stored = { ...value, passwordConfigured: true };
    });
    const save = createHostConfigSaver(draft, {
      setConfig,
      getConfig: async () => ({ ...stored }),
    });
    draft.password = 'first-password';
    const first = save();
    draft.password = 'second-password';
    const second = save();
    await vi.waitFor(() => expect(setConfig).toHaveBeenCalledOnce());
    firstWrite.resolve();
    await first;
    expect(draft.password).toBe('second-password');
    await second;
    expect(setConfig.mock.calls.map(([value]) => value.password)).toEqual([
      'first-password',
      'second-password',
    ]);
    expect(stored.password).toBe('second-password');
    expect(draft.passwordConfigured).toBe(true);
  });

  it('does not replace an unfinished edit when a save reads normalized settings back', async () => {
    const draft = config();
    const writing = gate();
    const save = createHostConfigSaver(draft, {
      setConfig: () => writing.promise,
      getConfig: async () => ({ ...config(), passwordConfigured: true }),
    });
    const saving = save();
    draft.password = 'still typing';
    draft.port = 31001;
    writing.resolve();
    await saving;
    expect(draft.password).toBe('still typing');
    expect(draft.port).toBe(31001);
    expect(draft.passwordConfigured).toBe(true);
  });

  it('reports a failed write and lets the next operation save successfully', async () => {
    const draft = config();
    const setConfig = vi
      .fn()
      .mockRejectedValueOnce(new Error('配置文件不可写'))
      .mockResolvedValue(undefined);
    const getConfig = vi.fn(async () => ({ ...draft }));
    const save = createHostConfigSaver(draft, { setConfig, getConfig });
    await expect(save()).rejects.toThrow('配置文件不可写');
    expect(getConfig).not.toHaveBeenCalled();
    draft.clipboard = false;
    await save();
    expect(setConfig).toHaveBeenLastCalledWith(expect.objectContaining({ clipboard: false }));
  });

  it('restores the stored password when saving an empty password preserves the old value', async () => {
    const draft = config();
    draft.password = '';
    const save = createHostConfigSaver(draft, {
      setConfig: async () => undefined,
      getConfig: async () => ({ ...config(), passwordConfigured: true }),
    });
    await save();
    expect(draft.password).toBe('old-password');
    expect(draft.passwordConfigured).toBe(true);
  });
});
