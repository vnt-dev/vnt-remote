import type { LinkConfig, ProfileStatus } from './api';

export function profileNameError(
  config: Pick<LinkConfig, 'id' | 'name'>,
  configs: Pick<LinkConfig, 'id' | 'name'>[],
): string {
  const name = config.name.trim();
  if (!name || [...name].length > 64) return '请填写 1–64 个字符的配置名称';
  return configs.some((other) => other.id !== config.id && other.name.trim() === name)
    ? '配置名称已存在'
    : '';
}
export function linkPhaseLabel(phase: string): string {
  return (
    (
      {
        stopped: '已停止',
        connecting: '正在连接',
        connected: '已连接',
        reconnecting: '正在重连',
        error: '连接失败',
      } as Record<string, string>
    )[phase] ?? '已停止'
  );
}
export function syncDeviceGroups(statuses: ProfileStatus[]) {
  return statuses.filter((s) => s.enabled).map((s) => ({ ...s, label: `同组设备-${s.name}` }));
}

export function profileConnectionError(
  config: Pick<LinkConfig, 'mode' | 'subscription' | 'servers' | 'networkCode' | 'connectionId'>,
): string {
  if (config.mode === 'subscription') return config.subscription.trim() ? '' : '请填写订阅地址';
  if (!config.servers.some((address) => address.trim())) return '请填写服务器地址';
  if (!config.networkCode.trim()) return '请填写房间编号';
  if (!config.connectionId.trim()) return '请填写连接 ID';
  if (!/^[0-9]{1,9}$/.test(config.connectionId)) return '连接 ID 必须为 1～9 位纯数字';
  return '';
}

export function connectionIdLabel(value: string): string {
  return /^[0-9]{1,9}$/.test(value) ? value : '';
}
