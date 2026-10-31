import { computed, ref, shallowRef, type Ref } from 'vue';
import {
  api,
  type ClientSessionInfo,
  type ClientStats,
  type RemoteScreen,
  type ScreenInputTarget,
} from '../services/api';
import { useDecoder, type StreamInfo } from './useDecoder';

export function useRemoteDecoder(
  canvas: Ref<HTMLCanvasElement | undefined>,
  session: ClientSessionInfo,
  onFatal: (reason: string) => void,
) {
  const screens = ref<RemoteScreen[]>(session.screens.map((screen) => ({ ...screen })));
  const detachedScreens = ref<number[]>([]);
  const owns = (id: number) =>
    session.windowStreamId ? id === session.windowStreamId : !detachedScreens.value.includes(id);
  const localScreens = computed(() => screens.value.filter((screen) => owns(screen.streamId)));
  const activeStreamId = ref(
    session.windowStreamId ?? screens.value.find((screen) => screen.available)?.streamId ?? 0,
  );
  const revision = ref(0);
  const busy = ref(false);
  const commandError = ref('');
  const screenStats = shallowRef<ClientStats | null>(null);
  const inputTarget = computed<ScreenInputTarget | undefined>(() => {
    const screen = screens.value.find((screen) => screen.streamId === activeStreamId.value);
    return screen
      ? { streamId: screen.streamId, generation: screen.generation, revision: revision.value }
      : undefined;
  });
  if (!screens.value.length) {
    return {
      ...useDecoder(canvas, onFatal, session.sessionId),
      screens,
      detachedScreens,
      localScreens,
      detach: async (_id: number) => {
        void _id;
      },
      activeStreamId,
      revision,
      busy,
      commandError,
      screenStats,
      inputTarget,
      select: async (id: number) => {
        void id;
      },
      closeScreen: async (id: number) => {
        void id;
      },
    };
  }

  type Decoder = ReturnType<typeof useDecoder>;
  const decoders = new Map<number, Decoder>();
  const statsByScreen = new Map<number, ClientStats>();
  const appliedConfigs = new Map<number, string>();
  const decoderRevision = ref(0);
  const transport = ref({ control: true, files: true, media: session.transport !== 'tcp' });
  let socket: WebSocket | null = null;
  let closed = false;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  let commandTimer: ReturnType<typeof setTimeout> | undefined;
  let attempts = 0;
  let confirmedActive = activeStreamId.value;
  let pendingCommand: { id: number; close: boolean } | null = null;
  let port = 0;
  let token = '';
  const retryTimers = new Map<number, ReturnType<typeof setTimeout>>();

  function create(screen: RemoteScreen) {
    if (
      !owns(screen.streamId) ||
      decoders.has(screen.streamId) ||
      !screen.available ||
      screen.width < 2 ||
      screen.height < 2 ||
      screen.fps < 1
    )
      return;
    const id = screen.streamId;
    const decoder = useDecoder(
      canvas,
      (reason) => {
        screen.reason = reason;
        if (!closed && screens.value.some((screen) => screen.streamId === id)) {
          retryTimers.set(
            id,
            setTimeout(() => {
              retryTimers.delete(id);
              if (closed || !screens.value.some((screen) => screen.streamId === id)) return;
              decoder.connect(port, token, screen);
              api
                .clientRequestKeyframe(session.sessionId, id, screen.generation)
                .catch(() => undefined);
            }, 2000),
          );
        }
      },
      session.sessionId,
      {
        streamId: id,
        generation: () => screen.generation,
        active: () => activeStreamId.value === id,
      },
    );
    decoders.set(id, decoder);
    decoder.connect(port, token, screen);
  }

  const selected = computed(() => {
    void decoderRevision.value;
    return decoders.get(activeStreamId.value);
  });
  const streamInfo = computed(() => selected.value?.streamInfo.value ?? null);
  const desktop = computed(() => {
    const screen = screens.value.find((screen) => screen.streamId === activeStreamId.value);
    return screen?.available
      ? (selected.value?.desktop.value ?? { ready: false, reason: '等待画面' })
      : { ready: false, reason: screen?.reason || '屏幕暂不可用' };
  });
  const frameReady = computed(() => selected.value?.frameReady.value ?? false);
  const lastError = computed(() => commandError.value || selected.value?.lastError.value || '');
  const emptyStats = {
    fps: 0,
    displayedFps: 0,
    bitrateKbps: 0,
    frames: 0,
    displayed: 0,
    droppedDecoded: 0,
    queueSize: 0,
    lagMs: 0,
    p95Ms: 0,
  };
  const stats = computed(() => selected.value?.stats.value ?? emptyStats);

  function updateStats() {
    screenStats.value = statsByScreen.get(activeStreamId.value) ?? null;
  }
  function message(data: string | ArrayBuffer) {
    if (typeof data !== 'string') {
      if (data.byteLength < 17) return;
      const view = new DataView(data);
      const id = view.getUint32(0),
        generation = view.getUint32(4);
      const screen = screens.value.find((screen) => screen.streamId === id);
      if (screen?.generation === generation && screen.available) {
        attempts = 0;
        decoders.get(id)?.handleMessage(data.slice(8));
      }
      return;
    }
    let msg;
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }
    if (msg.type === 'screensState') {
      if (msg.reset) {
        appliedConfigs.clear();
        for (const decoder of decoders.values())
          decoder.handleMessage(JSON.stringify({ type: 'videoReset' }));
      }
      if (Number(msg.revision) < revision.value || !Array.isArray(msg.screens)) return;
      detachedScreens.value = Array.isArray(msg.detachedScreens)
        ? msg.detachedScreens.map(Number)
        : [];
      const ids = new Set<number>(msg.screens.map((screen: RemoteScreen) => screen.streamId));
      for (const [id, decoder] of decoders)
        if (!ids.has(id) || !owns(id)) {
          decoder.close();
          decoders.delete(id);
          clearTimeout(retryTimers.get(id));
          retryTimers.delete(id);
          statsByScreen.delete(id);
          appliedConfigs.delete(id);
        }
      // Keep the same metadata objects: each decoder's generation getter uses it.
      screens.value = msg.screens.map((next: RemoteScreen) => {
        const old = screens.value.find((old) => old.streamId === next.streamId);
        return old ? Object.assign(old, next) : next;
      });
      revision.value = Number(msg.revision);
      confirmedActive = Number(msg.activeStreamId);
      const acknowledged =
        !pendingCommand ||
        (pendingCommand.close
          ? !ids.has(pendingCommand.id)
          : confirmedActive === pendingCommand.id);
      if (acknowledged) {
        activeStreamId.value =
          session.windowStreamId ??
          (owns(confirmedActive) && ids.has(confirmedActive)
            ? confirmedActive
            : ids.has(activeStreamId.value) && owns(activeStreamId.value)
              ? activeStreamId.value
              : (msg.screens.find((screen: RemoteScreen) => owns(screen.streamId))?.streamId ?? 0));
        pendingCommand = null;
        busy.value = false;
        clearTimeout(commandTimer);
      }
      for (const screen of screens.value) create(screen);
      decoderRevision.value++;
      updateStats();
      if (msg.transportState) {
        for (const key of ['control', 'files', 'media'] as const)
          if (typeof msg.transportState[key] === 'boolean')
            transport.value[key] = msg.transportState[key];
      }
      if (msg.streams)
        for (const value of Object.values(msg.streams) as Array<{
          config?: unknown;
          desktop?: unknown;
        }>) {
          if (value.config) message(JSON.stringify(value.config));
          if (value.desktop) message(JSON.stringify(value.desktop));
        }
    } else if (msg.type === 'transportState') {
      const key = ['control', 'files', 'media'][Number(msg.kind)] as
        keyof typeof transport.value | undefined;
      if (key) transport.value[key] = Boolean(msg.available);
      for (const decoder of decoders.values()) decoder.handleMessage(data);
    } else if (msg.type === 'videoReset') {
      for (const decoder of decoders.values()) decoder.handleMessage(data);
    } else if (msg.type === 'screenStats') {
      statsByScreen.set(Number(msg.streamId), { ...msg, sessionId: session.sessionId });
      updateStats();
    } else {
      const id = Number(msg.streamId);
      const screen = screens.value.find((screen) => screen.streamId === id);
      if (!screen || Number(msg.generation) < screen.generation) return;
      if (msg.type === 'streamInfo') {
        if (appliedConfigs.get(id) === data) return;
        appliedConfigs.set(id, data);
        Object.assign(screen, {
          width: msg.width,
          height: msg.height,
          fps: msg.fps,
          generation: msg.generation,
        });
      }
      decoders.get(id)?.handleMessage(data);
    }
  }

  function openSocket() {
    if (closed) return;
    socket = new WebSocket(`ws://127.0.0.1:${port}/?t=${token}`);
    socket.binaryType = 'arraybuffer';
    socket.onmessage = (event) => {
      message(event.data);
    };
    socket.onopen = () => {
      appliedConfigs.clear();
      for (const decoder of decoders.values())
        decoder.handleMessage(JSON.stringify({ type: 'videoReset' }));
    };
    socket.onclose = () => {
      if (closed) return;
      if (++attempts > 5) {
        onFatal('视频桥持续断开');
        api.clientDisconnect(session.sessionId).catch(() => undefined);
        return;
      }
      reconnectTimer = setTimeout(openSocket, 1000);
    };
  }

  function connect(wsPort: number, wsToken: string, _initial: StreamInfo) {
    void _initial;
    port = wsPort;
    token = wsToken;
    closed = false;
    for (const screen of screens.value) create(screen);
    decoderRevision.value++;
    openSocket();
  }
  async function command(id: number, close: boolean) {
    if (session.windowStreamId || busy.value || !transport.value.control) return;
    if (!owns(id)) {
      if (!close) await detach(id);
      return;
    }
    if (close && localScreens.value.length <= 1) return;
    const previous = activeStreamId.value;
    const pending = { id, close };
    pendingCommand = pending;
    busy.value = true;
    commandError.value = '';
    if (!close) {
      activeStreamId.value = id;
      updateStats();
    }
    try {
      await api.clientScreenCommand(session.sessionId, id, revision.value, close);
      if (pendingCommand === pending)
        commandTimer = setTimeout(() => {
          pendingCommand = null;
          activeStreamId.value = owns(confirmedActive)
            ? confirmedActive
            : (localScreens.value.find((screen) => screen.streamId === activeStreamId.value)
                ?.streamId ??
              localScreens.value[0]?.streamId ??
              0);
          updateStats();
          busy.value = false;
          commandError.value = '屏幕操作尚未确认，请重试';
        }, 5000);
    } catch (error) {
      pendingCommand = null;
      activeStreamId.value = previous;
      updateStats();
      busy.value = false;
      commandError.value = String(error);
    }
  }
  async function detach(id: number) {
    if (session.windowStreamId || busy.value || !transport.value.control) return;
    if (owns(id) && localScreens.value.length <= 1) return;
    busy.value = true;
    commandError.value = '';
    try {
      await api.detachScreen(session.sessionId, id);
    } catch (error) {
      commandError.value = String(error);
    } finally {
      busy.value = false;
    }
  }
  function close() {
    closed = true;
    clearTimeout(reconnectTimer);
    clearTimeout(commandTimer);
    for (const timer of retryTimers.values()) clearTimeout(timer);
    retryTimers.clear();
    for (const decoder of decoders.values()) decoder.close();
    decoders.clear();
    if (socket) {
      socket.onclose = null;
      socket.close();
      socket = null;
    }
  }
  return {
    streamInfo,
    desktop,
    transport,
    frameReady,
    stats,
    lastError,
    connect,
    close,
    screens,
    detachedScreens,
    localScreens,
    detach,
    activeStreamId,
    revision,
    busy,
    commandError,
    screenStats,
    inputTarget,
    select: (id: number) => command(id, false),
    closeScreen: (id: number) => command(id, true),
  };
}
