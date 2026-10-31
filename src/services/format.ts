// 字节/速率的人类可读格式化（传输面板用）。

/** 1024 进制字节数 → "1.5 MB"。 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  // 小于 10 保留一位小数，其余取整：面板宽度有限
  const digits = unit === 0 || value >= 10 ? 0 : 1;
  return `${value.toFixed(digits)} ${units[unit]}`;
}

/** kbps → "4.5 Mbps" / "800 Kbps"。 */
export function formatSpeed(kbps: number): string {
  if (!Number.isFinite(kbps) || kbps <= 0) return '';
  return kbps >= 1000 ? `${(kbps / 1000).toFixed(1)} Mbps` : `${Math.round(kbps)} Kbps`;
}

/** 秒数 → "18 秒" / "2 分 30 秒" / "1 小时 4 分"（复制对话框的剩余时间）。 */
export function formatEta(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return '';
  const total = Math.ceil(seconds);
  if (total < 60) return `${total} 秒`;
  const minutes = Math.floor(total / 60);
  if (minutes < 60) {
    const rest = total % 60;
    return rest === 0 ? `${minutes} 分` : `${minutes} 分 ${rest} 秒`;
  }
  const hours = Math.floor(minutes / 60);
  return `${hours} 小时 ${minutes % 60} 分`;
}

/** 进度百分比（0–100，整数）；总量未知时为 0。 */
export function progressPercent(completed: number, total: number): number {
  if (!Number.isFinite(total) || total <= 0) return 0;
  const percent = (completed / total) * 100;
  return Math.max(0, Math.min(100, Math.round(percent)));
}
