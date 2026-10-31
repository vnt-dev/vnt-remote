import { afterEach, describe, expect, it, vi } from 'vitest';
import { nextTick, ref } from 'vue';
import { api, type ScreenInputTarget } from '../services/api';
import { useScreenVisibility } from './useScreenVisibility';

vi.mock('../services/api', () => ({ api: { clientViewScreen: vi.fn() } }));
const target = (streamId: number, generation = 1): ScreenInputTarget => ({
  streamId,
  generation,
  revision: 1,
});
let stop: (() => void) | undefined;
afterEach(() => {
  stop?.();
  vi.useRealTimers();
  vi.resetAllMocks();
});

async function flush() {
  await nextTick();
  await Promise.resolve();
  await Promise.resolve();
}

describe('painted screen reports', () => {
  it('reports actual selection and generation changes without depending on input revisions', async () => {
    vi.mocked(api.clientViewScreen).mockResolvedValue(undefined);
    const current = ref<ScreenInputTarget | undefined>(target(1));
    stop = useScreenVisibility(42, current);
    await flush();
    expect(api.clientViewScreen).toHaveBeenLastCalledWith(42, 1, 1);
    current.value = target(2);
    await flush();
    current.value = target(2, 3);
    await flush();
    expect(api.clientViewScreen).toHaveBeenLastCalledWith(42, 2, 3);
    expect(api.clientViewScreen).toHaveBeenCalledTimes(3);
    current.value = { ...target(2, 3), revision: 10 };
    await flush();
    expect(api.clientViewScreen).toHaveBeenCalledTimes(3);
  });

  it('serializes IPC and coalesces rapid switches to the latest view', async () => {
    let finish!: () => void;
    vi.mocked(api.clientViewScreen)
      .mockImplementationOnce(
        () =>
          new Promise<void>((resolve) => {
            finish = resolve;
          }),
      )
      .mockResolvedValue(undefined);
    const current = ref<ScreenInputTarget | undefined>(target(1));
    stop = useScreenVisibility(42, current);
    current.value = target(2);
    await flush();
    current.value = target(3);
    await flush();
    expect(api.clientViewScreen).toHaveBeenCalledTimes(1);
    finish();
    await flush();
    expect(api.clientViewScreen).toHaveBeenCalledTimes(2);
    expect(api.clientViewScreen).toHaveBeenLastCalledWith(42, 3, 1);
  });

  it('retries transient IPC failures and stops after teardown', async () => {
    vi.useFakeTimers();
    vi.mocked(api.clientViewScreen)
      .mockRejectedValueOnce(new Error('not ready'))
      .mockResolvedValue(undefined);
    stop = useScreenVisibility(42, ref(target(1)));
    await flush();
    await vi.advanceTimersByTimeAsync(200);
    expect(api.clientViewScreen).toHaveBeenCalledTimes(2);
    stop();
    await vi.advanceTimersByTimeAsync(1000);
    expect(api.clientViewScreen).toHaveBeenCalledTimes(2);
  });

  it('never retries an obsolete report after the view has disappeared', async () => {
    vi.useFakeTimers();
    let reject!: (reason: Error) => void;
    vi.mocked(api.clientViewScreen).mockImplementationOnce(
      () =>
        new Promise<void>((_, fail) => {
          reject = fail;
        }),
    );
    const current = ref<ScreenInputTarget | undefined>(target(1));
    stop = useScreenVisibility(42, current);
    current.value = undefined;
    await flush();
    reject(new Error('closed'));
    await flush();
    await vi.advanceTimersByTimeAsync(1000);
    expect(api.clientViewScreen).toHaveBeenCalledTimes(1);
  });
});
