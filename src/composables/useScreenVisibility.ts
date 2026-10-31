import { watch, type Ref } from 'vue';
import { api, type ScreenInputTarget } from '../services/api';

/** Only report the painted stream. Native Rust polling owns timing/minimization. */
export function useScreenVisibility(sessionId: number, target: Ref<ScreenInputTarget | undefined>) {
  let closed = false;
  let running = false;
  let latest: ScreenInputTarget | undefined;
  let retry: ReturnType<typeof setTimeout> | undefined;
  async function send() {
    if (running || closed || !latest) return;
    running = true;
    const current = latest;
    latest = undefined;
    try {
      await api.clientViewScreen(sessionId, current.streamId, current.generation);
    } catch {
      if (
        target.value?.streamId === current.streamId &&
        target.value.generation === current.generation
      )
        latest ??= current;
      if (!closed && latest)
        retry = setTimeout(() => {
          retry = undefined;
          void send();
        }, 200);
    } finally {
      running = false;
    }
    if (latest && !retry && !closed) void send();
  }
  const stop = watch(
    [() => target.value?.streamId, () => target.value?.generation],
    () => {
      latest = target.value;
      clearTimeout(retry);
      retry = undefined;
      void send();
    },
    { immediate: true },
  );
  return () => {
    closed = true;
    stop();
    clearTimeout(retry);
  };
}
