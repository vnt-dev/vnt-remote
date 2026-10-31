// WebCodecs 视频解码 + Canvas 渲染
// 数据源：回环 WS（文本帧 streamInfo / 二进制帧视频）

import { ref, type Ref } from 'vue';
import { api } from '../services/api';
import { decoderConfig, loadHwDecodePref, selectDecoderConfig } from '../services/codec';

export interface StreamInfo {
  codec: string;
  width: number;
  height: number;
  fps: number;
  hardware: boolean;
}

export interface DecoderStats {
  /** 解码输出帧率（进解码器的帧被吐出来的速率） */
  fps: number;
  /** 实际显示帧率：真正被 drawImage 画出去的帧。
   *  与 fps 的差就是"解出来了但没上屏"的部分——那部分是纯延迟。 */
  displayedFps: number;
  bitrateKbps: number;
  /** 累计解码帧数 */
  frames: number;
  /** 累计显示帧数 */
  displayed: number;
  /** 解码了却被更新的帧替换掉、从未上屏的帧数 */
  droppedDecoded: number;
  /** 解码队列深度（条数） */
  queueSize: number;
  /** 画面年龄：最新到达帧与最新解码输出的 pts 差（ms）。
   *  比队列条数更直接的延迟指标——条数在不同帧率下含义不同。 */
  lagMs: number;
  /** 前端可观测的端到端 P95 延迟：帧离线到达 → 上屏（ms） */
  p95Ms: number;
}

/** 待解码帧覆盖的媒体时间上限；已输出画面与下一帧之间的静止时间不算积压。 */
const MAX_FRAME_LAG_MS = 100;

export function useDecoder(
  canvas: Ref<HTMLCanvasElement | undefined>,
  onFatal?: (reason: string) => void,
  sessionId?: number,
  external?: { streamId: number; generation: () => number; active: () => boolean },
) {
  const streamInfo: Ref<StreamInfo | null> = ref(null);
  const desktop = ref({ ready: true, reason: '' });
  const frameReady = ref(false);
  const stats: Ref<DecoderStats> = ref({
    fps: 0,
    displayedFps: 0,
    bitrateKbps: 0,
    frames: 0,
    displayed: 0,
    droppedDecoded: 0,
    queueSize: 0,
    lagMs: 0,
    p95Ms: 0,
  });
  const lastError: Ref<string> = ref('');
  const transport = ref({ control: true, files: true, media: true });

  let ws: WebSocket | null = null;
  let decoder: VideoDecoder | null = null;
  let ctx: CanvasRenderingContext2D | null = null;
  let latestFrame: VideoFrame | null = null;
  let renderedFrame: VideoFrame | null = null;
  let wasActive = false;
  let rafId = 0;
  let statFrames = 0;
  let statDisplayed = 0;
  let statBytes = 0;
  let statDroppedDecoded = 0;
  let statWindowStart = performance.now();
  /** 最近一次延迟采样（帧离线到达 → 上屏），用于 P95 */
  const latencySamples: number[] = [];
  const LATENCY_WINDOW = 120;
  /** 最新到达帧 / 最新解码输出的 pts（服务端会话相对时间戳，ms） */
  let lastArrivedPts = 0;
  let lastDecodedPts = 0;
  /** 与 latestFrame 配对的接收时刻（上屏时算延迟用） */
  let latestFrameRecvAt: number | null = null;
  // 解码异步完成：按时间戳匹配接收时刻，不能使用最近一个网络包的时间。
  const receivedAt = new Map<number, number>();
  const MAX_PENDING_TIMESTAMPS = 128;
  let closed = false;
  let statsTimer: ReturnType<typeof setTimeout> | undefined;
  let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  let stableFrames = 0;
  const requestKeyframe = () =>
    api
      .clientRequestKeyframe(sessionId, external?.streamId, external?.generation())
      .catch(() => undefined);
  // 解码链路损坏/积压丢帧后：在收到关键帧前只透传关键帧，并请求恢复
  let needsKeyframe = false;
  // 「硬件解码」开关（默认开）：先探测硬件配置，不支持时选用软件配置。
  // WebCodecs 不保证 prefer-hardware 会自动回退到软件。
  let acceleration: HardwareAcceleration = loadHwDecodePref()
    ? 'prefer-hardware'
    : 'prefer-software';
  const supportedConfigs = new Map<string, VideoDecoderConfig>();
  let decoderGeneration = 0;
  let streamGeneration: number | undefined;
  let lastOutputAt = performance.now();
  let pendingSince: number | null = null;

  function recoverDecoder(reason: string) {
    if (closed) return;
    lastError.value = reason;
    if (acceleration === 'prefer-software') {
      close();
      onFatal?.(`视频解码失败：${reason}`);
      if (!external) api.clientDisconnect(sessionId).catch(() => undefined);
      return;
    }
    acceleration = 'prefer-software';
    pendingSince = null;
    if (streamInfo.value) configureDecoder(streamInfo.value);
    requestKeyframe();
  }

  function ensureCanvas(w: number, h: number) {
    const c = canvas.value;
    if (!c || (external && !external.active())) return true;
    try {
      // Decode at source resolution, but paint only the pixels the window can
      // display. A small 4K preview should not redraw an invisible 4K canvas.
      const parent = c.parentElement;
      const scale =
        parent && parent.clientWidth > 0 && parent.clientHeight > 0
          ? Math.min(1, parent.clientWidth / w, parent.clientHeight / h)
          : 1;
      const displayWidth = Math.max(1, Math.floor(w * scale));
      const displayHeight = Math.max(1, Math.floor(h * scale));
      const pixelRatio = Math.max(1, globalThis.devicePixelRatio || 1);
      const paintWidth = Math.min(w, Math.round(displayWidth * pixelRatio));
      const paintHeight = Math.min(h, Math.round(displayHeight * pixelRatio));
      if (c.style) {
        const cssWidth = `${displayWidth}px`,
          cssHeight = `${displayHeight}px`;
        if (c.style.width !== cssWidth) c.style.width = cssWidth;
        if (c.style.height !== cssHeight) c.style.height = cssHeight;
      }
      if (c.width !== paintWidth || c.height !== paintHeight) {
        c.width = paintWidth;
        c.height = paintHeight;
      }
      ctx = ctx ?? c.getContext('2d', { alpha: false, desynchronized: true });
      if (!ctx) throw new Error('无法创建 Canvas 2D 绘图上下文');
      return true;
    } catch (error) {
      lastError.value = `无法显示远程画面：${error}`;
      const reason = lastError.value;
      close();
      onFatal?.(reason);
      if (!external) api.clientDisconnect(sessionId).catch(() => undefined);
      return false;
    }
  }

  function renderLoop() {
    if (closed) return;
    const active = external?.active() ?? true;
    if (latestFrame && active && (!external || latestFrame !== renderedFrame || !wasActive)) {
      const info = streamInfo.value;
      if (info && !ensureCanvas(info.width, info.height)) return;
      if (!ctx) {
        rafId = requestAnimationFrame(renderLoop);
        return;
      }
      const frame = latestFrame;
      renderedFrame = frame;
      if (!external) latestFrame = null;
      const now = performance.now();
      const c = canvas.value!;
      // VideoFrame 的显示尺寸可能与协商尺寸不同（驱动的裁剪/像素比例元数据）。
      // 显式铺满画布，避免按自然尺寸只画在左上角。
      try {
        ctx.drawImage(frame, 0, 0, c.width, c.height);
      } catch (error) {
        recoverDecoder(`显示视频失败: ${error}`);
        if (!closed) rafId = requestAnimationFrame(renderLoop);
        return;
      } finally {
        if (!external) frame.close();
      }
      frameReady.value = true;
      statDisplayed++;
      if (latestFrameRecvAt !== null) {
        latencySamples.push(now - latestFrameRecvAt);
        if (latencySamples.length > LATENCY_WINDOW) latencySamples.shift();
      }
    }
    wasActive = active;
    rafId = requestAnimationFrame(renderLoop);
  }

  function resetDecoder(preserveFrame = false) {
    decoderGeneration++;
    receivedAt.clear();
    if (!preserveFrame) {
      latestFrame?.close();
      latestFrame = null;
      renderedFrame = null;
    }
    if (decoder) {
      try {
        decoder.close();
      } catch {
        /* 已关闭 */
      }
      decoder = null;
    }
  }

  function clearDesktopFrame() {
    frameReady.value = false;
    resetDecoder();
    const element = canvas.value;
    if (!external || external.active())
      element?.getContext('2d')?.clearRect(0, 0, element.width, element.height);
  }

  function configureDecoder(info: StreamInfo) {
    // Same-desktop decoder recovery must not raise the desktop-switch veil.
    // Retain each screen's last frame so it can also be repainted after tab changes.
    const generation = external?.generation();
    const preserveFrame = !!external && streamGeneration === generation;
    if (external && !preserveFrame) clearDesktopFrame();
    else resetDecoder(preserveFrame);
    streamGeneration = generation;
    const decoderEpoch = decoderGeneration;
    needsKeyframe = true;
    const config = decoderConfig(info.codec, info.width, info.height, acceleration);
    const configKey = JSON.stringify(config);
    const applyConfig = (supported: VideoDecoderConfig) => {
      if (closed || decoderEpoch !== decoderGeneration) return;
      supportedConfigs.set(configKey, supported);
      acceleration = supported.hardwareAcceleration ?? 'no-preference';
      createDecoder(supported, decoderEpoch);
    };
    if (typeof VideoDecoder === 'undefined') {
      recoverDecoder('当前环境不支持 WebCodecs 视频解码');
      return;
    }
    const cached = supportedConfigs.get(configKey);
    if (cached) applyConfig(cached);
    else {
      void selectDecoderConfig(config).then(
        (supported) => {
          applyConfig(supported);
          if (!closed && decoderEpoch === decoderGeneration) requestKeyframe();
        },
        (error) => {
          if (!closed && decoderEpoch === decoderGeneration)
            recoverDecoder(`配置解码器失败: ${error}`);
        },
      );
    }
  }

  function createDecoder(config: VideoDecoderConfig, decoderEpoch: number) {
    try {
      decoder = new VideoDecoder({
        output: (frame: VideoFrame) => {
          if (closed || decoderEpoch !== decoderGeneration) {
            frame.close();
            return;
          }
          lastOutputAt = performance.now();
          pendingSince = null;
          if (latestFrame) {
            latestFrame.close();
            if (latestFrame !== renderedFrame) statDroppedDecoded++;
          }
          latestFrame = frame;
          if (external) frameReady.value = true;
          latestFrameRecvAt = receivedAt.get(frame.timestamp) ?? null;
          // 输出可能跳过输入帧；比当前输出更旧的时间戳已不再代表待解码工作。
          for (const timestamp of receivedAt.keys()) {
            if (timestamp <= frame.timestamp) receivedAt.delete(timestamp);
          }
          statFrames++;
          lastError.value = '';
          stableFrames++;
          lastDecodedPts = frame.timestamp / 1000;
          // Reset only after two seconds of decoded video, not a successful WS handshake.
          if (stableFrames >= Math.max(1, streamInfo.value?.fps ?? 30) * 2) {
            reconnectAttempts = 0;
          }
        },
        error: (e: DOMException) => {
          if (decoderEpoch === decoderGeneration) recoverDecoder(`解码错误: ${e.message}`);
        },
      });
      decoder.configure(config);
    } catch (e) {
      recoverDecoder(`配置解码器失败: ${e}`);
    }
  }

  function handleBinary(buf: ArrayBuffer) {
    if (!desktop.value.ready || buf.byteLength < 9) return;
    const dv = new DataView(buf);
    const keyframe = dv.getUint8(0) !== 0;
    const pts = dv.getUint32(1);
    const len = dv.getUint32(5);
    if (len === 0 || len > 8 * 1024 * 1024 || 9 + len !== buf.byteLength) return;
    const data = new Uint8Array(buf, 9, len);
    statBytes += len;
    lastArrivedPts = pts;
    if (!decoder) return;
    // 链路恢复门控：处于 needsKeyframe 时只放行关键帧（丢弃的 delta
    // 会造成参考链断裂，继续送后续 delta 只会花屏，必须等 IDR）
    if (needsKeyframe && !keyframe) return;
    const oldestPending = receivedAt.entries().next().value;
    if (
      decoder.decodeQueueSize > 8 ||
      (oldestPending &&
        pts - oldestPending[0] / 1000 > MAX_FRAME_LAG_MS &&
        performance.now() - oldestPending[1] > MAX_FRAME_LAG_MS)
    ) {
      // Media timestamps can jump after idle capture or arrive in a burst.
      // Only age work that has actually waited here, rather than resetting a
      // healthy decoder before it has had time to produce its first output.
      // 重建解码器真正取消旧队列，再等待 IDR；只停止送新帧不会清除积压。
      needsKeyframe = true;
      if (streamInfo.value) configureDecoder(streamInfo.value);
      if (closed || !decoder) return;
      if (!keyframe) {
        requestKeyframe();
        return;
      }
    }
    // Keyframes also consume decoding work. Shed the old queue before accepting
    // a fresh IDR, rather than allowing an IDR burst to grow it without bounds.
    if (keyframe) needsKeyframe = false;
    try {
      pendingSince ??= performance.now();
      receivedAt.set(pts * 1000, performance.now());
      // 解码器可能不输出所有输入帧；限制映射大小，避免长期运行累积。
      if (receivedAt.size > MAX_PENDING_TIMESTAMPS) {
        receivedAt.delete(receivedAt.keys().next().value!);
      }
      decoder.decode(
        new EncodedVideoChunk({
          type: keyframe ? 'key' : 'delta',
          timestamp: pts * 1000,
          data,
        }),
      );
    } catch (e) {
      // 送帧失败（如队列关闭/状态异常）：请求关键帧并重建解码器
      recoverDecoder(`送解码失败: ${e}`);
    }
  }

  function statsLoop() {
    if (closed) return;
    const now = performance.now();
    // The control queue may have dropped the first request; retry while waiting
    // for a keyframe instead of relying on a long GOP on an idle desktop.
    if (needsKeyframe && now - statWindowStart >= 1000) requestKeyframe();
    // 部分显卡驱动既不输出帧也不报错；不能一直显示黑屏并反复清队列。
    if (pendingSince !== null && now - Math.max(pendingSince, lastOutputAt) > 3000) {
      recoverDecoder('收到视频后 3 秒没有解码输出');
      if (closed) return;
    }
    const el = (now - statWindowStart) / 1000;
    if (el >= 1) {
      const sorted = [...latencySamples].sort((a, b) => a - b);
      stats.value = {
        fps: Math.round(statFrames / el),
        displayedFps: Math.round(statDisplayed / el),
        bitrateKbps: Math.round((statBytes * 8) / 1000 / el),
        frames: (stats.value.frames ?? 0) + statFrames,
        displayed: (stats.value.displayed ?? 0) + statDisplayed,
        droppedDecoded: (stats.value.droppedDecoded ?? 0) + statDroppedDecoded,
        queueSize: decoder?.decodeQueueSize ?? 0,
        lagMs: Math.max(0, lastArrivedPts - lastDecodedPts),
        p95Ms: sorted.length ? Math.round(sorted[Math.floor(sorted.length * 0.95)] ?? 0) : 0,
      };
      statFrames = 0;
      statDisplayed = 0;
      statDroppedDecoded = 0;
      statBytes = 0;
      statWindowStart = now;
    }
    statsTimer = setTimeout(statsLoop, 1000);
  }

  // 重连所需参数与状态
  let lastWsPort = 0;
  let lastToken = '';
  let lastInitial: StreamInfo | null = null;
  let reconnectAttempts = 0;
  const MAX_RECONNECT = 5;
  // 渲染循环/统计定时器只启动一次：重连不得重复创建，
  // 否则每次断线重连都叠加一条 rAF 链和一个 setTimeout 链
  let loopsStarted = false;

  function ensureLoops() {
    if (loopsStarted || closed) return;
    loopsStarted = true;
    rafId = requestAnimationFrame(renderLoop);
    statsLoop();
  }

  function handleMessage(data: string | ArrayBuffer) {
    // A failed screen still receives shared bridge messages until its retry.
    // Only connect() may reopen it and restart rendering/statistics loops.
    if (closed) return;
    if (typeof data === 'string') {
      try {
        const msg = JSON.parse(data);
        if (msg.type === 'transportState') {
          const key = ['control', 'files', 'media'][Number(msg.kind)] as
            keyof typeof transport.value | undefined;
          if (key) transport.value[key] = Boolean(msg.available);
          if (Number(msg.kind) === 2) {
            clearDesktopFrame();
            if (msg.available && streamInfo.value) configureDecoder(streamInfo.value);
          }
        } else if (msg.type === 'desktopState') {
          const wasReady = desktop.value.ready;
          desktop.value = { ready: Boolean(msg.ready), reason: String(msg.reason || '') };
          if (!msg.ready) {
            clearDesktopFrame();
          } else if (!wasReady && streamInfo.value) configureDecoder(streamInfo.value);
        } else if (msg.type === 'streamInfo') {
          if (msg.transportState) {
            for (const key of ['control', 'files', 'media'] as const) {
              if (typeof msg.transportState[key] === 'boolean')
                transport.value[key] = msg.transportState[key];
            }
          }
          if (typeof msg.desktopReady === 'boolean')
            desktop.value = { ready: msg.desktopReady, reason: String(msg.desktopReason || '') };
          if (!desktop.value.ready) clearDesktopFrame();
          streamInfo.value = msg;
          lastInitial = msg;
          if (!ensureCanvas(msg.width, msg.height)) return;
          if (desktop.value.ready) configureDecoder(msg);
          if (!closed && desktop.value.ready) requestKeyframe();
        } else if (msg.type === 'videoReset' && streamInfo.value) {
          configureDecoder(streamInfo.value);
          if (!closed) requestKeyframe();
        }
      } catch {
        /* 忽略 */
      }
    } else {
      handleBinary(data as ArrayBuffer);
    }
  }

  function connect(wsPort: number, token: string, initial: StreamInfo) {
    clearTimeout(reconnectTimer);
    closeSocket();
    closed = false;
    frameReady.value = false;
    pendingSince = null;
    lastOutputAt = performance.now();
    lastWsPort = wsPort;
    lastToken = token;
    lastInitial = initial;
    streamInfo.value = initial;
    if (!ensureCanvas(initial.width, initial.height)) return;
    configureDecoder(initial);
    if (closed) return;
    ensureLoops();

    if (external) return;
    stableFrames = 0;
    ws = new WebSocket(`ws://127.0.0.1:${wsPort}/?t=${token}`);
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => {
      requestKeyframe();
    };
    ws.onmessage = (ev) => handleMessage(ev.data);
    ws.onerror = () => {
      lastError.value = '视频桥连接失败';
    };
    // 桥主动关闭（发送停滞超时）或异常断开：必须处理——
    // 否则画面冻结且无任何提示。重连若干次后明确结束会话。
    ws.onclose = () => {
      if (closed) return;
      if (reconnectAttempts < MAX_RECONNECT) {
        reconnectAttempts += 1;
        needsKeyframe = true;
        requestKeyframe();
        lastError.value = `视频通道断开，重连中（${reconnectAttempts}/${MAX_RECONNECT}）…`;
        reconnectTimer = setTimeout(() => {
          if (!closed && lastInitial) {
            connect(lastWsPort, lastToken, lastInitial);
          }
        }, 1000);
      } else {
        lastError.value = '视频通道持续断开，结束会话';
        const reason = lastError.value;
        close();
        onFatal?.(reason);
        if (!external) api.clientDisconnect(sessionId).catch(() => undefined);
      }
    };
  }

  function closeSocket() {
    if (!ws) return;
    ws.onclose = null;
    ws.onmessage = null;
    ws.onopen = null;
    ws.onerror = null;
    try {
      ws.close();
    } catch {
      /* Already closed. */
    }
    ws = null;
  }

  function close() {
    closed = true;
    frameReady.value = false;
    clearTimeout(statsTimer);
    clearTimeout(reconnectTimer);
    loopsStarted = false;
    cancelAnimationFrame(rafId);
    resetDecoder();
    if (latestFrame) {
      latestFrame.close();
      latestFrame = null;
    }
    closeSocket();
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
    handleMessage,
  };
}
