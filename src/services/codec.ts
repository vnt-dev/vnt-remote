// WebCodecs 解码能力探测（连接时上报服务端参与协商）

export interface CodecSupport {
  h264: boolean;
  hevc: boolean;
  av1: boolean;
}

export function codecString(codec: string): string {
  // 覆盖 4K@30；旧的 level 4 仅能描述 1080p，探测成功并不代表能解远端 4K。
  switch (codec) {
    case 'hevc':
      return 'hev1.1.6.L153.B0';
    case 'av1':
      return 'av01.0.12M.08';
    default:
      return 'avc1.640033';
  }
}

export function decoderConfig(
  codec: string,
  width: number,
  height: number,
  hardwareAcceleration: HardwareAcceleration = 'no-preference',
): VideoDecoderConfig {
  return {
    codec: codecString(codec),
    codedWidth: width,
    codedHeight: height,
    optimizeForLatency: true,
    hardwareAcceleration,
  };
}

/** Probe the exact playback configuration before configure() can fail asynchronously. */
export async function selectDecoderConfig(config: VideoDecoderConfig): Promise<VideoDecoderConfig> {
  const accelerations: HardwareAcceleration[] =
    config.hardwareAcceleration === 'prefer-hardware'
      ? ['prefer-hardware', 'prefer-software']
      : [config.hardwareAcceleration ?? 'no-preference'];
  for (const hardwareAcceleration of accelerations) {
    const candidate = { ...config, hardwareAcceleration };
    try {
      if ((await VideoDecoder.isConfigSupported(candidate)).supported) return candidate;
    } catch {
      // A driver may reject a preference; continue with the remaining candidate.
    }
  }
  throw new Error(`不支持 ${config.codec} 视频解码（${config.codedWidth}×${config.codedHeight}）`);
}

export async function probeCodecSupport(): Promise<CodecSupport> {
  const check = async (codec: string) => {
    if (typeof VideoDecoder === 'undefined') return false;
    try {
      const support = await VideoDecoder.isConfigSupported(decoderConfig(codec, 3840, 2160));
      return !!support.supported;
    } catch {
      return false;
    }
  };
  const [h264, hevc, av1] = await Promise.all([check('h264'), check('hevc'), check('av1')]);
  return { h264, hevc, av1 };
}

/** 返回可解码列表（按能力偏好排序，供 client_connect 上报） */
export async function supportedCodecList(): Promise<string[]> {
  const s = await probeCodecSupport();
  const list: string[] = [];
  if (s.av1) list.push('av1');
  if (s.hevc) list.push('hevc');
  if (s.h264) list.push('h264');
  return list;
}

/** 仅上报启用了硬件解码、且通过硬件配置探测的编码。 */
export async function supportedHardwareCodecList(codecs: string[]): Promise<string[]> {
  if (!loadHwDecodePref() || typeof VideoDecoder === 'undefined') return [];
  const supported = await Promise.all(
    codecs.map(async (codec) => {
      try {
        const result = await VideoDecoder.isConfigSupported(
          decoderConfig(codec, 3840, 2160, 'prefer-hardware'),
        );
        return !!result.supported;
      } catch {
        return false;
      }
    }),
  );
  return codecs.filter((_, index) => supported[index]);
}

// 硬件解码偏好（客户端「高级选项」开关）：主窗口写 localStorage，
// 控制窗口同源共享，创建解码器时读取。
const HW_DECODE_KEY = 'vnt.hwDecode';

export function loadHwDecodePref(): boolean {
  try {
    return localStorage.getItem(HW_DECODE_KEY) !== '0';
  } catch {
    return true;
  }
}

export function saveHwDecodePref(on: boolean) {
  try {
    localStorage.setItem(HW_DECODE_KEY, on ? '1' : '0');
  } catch {
    // 存储不可用时仅本次生效
  }
}
