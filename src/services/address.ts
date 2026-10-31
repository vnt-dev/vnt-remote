export const DEFAULT_PORT = 29875;
export const ADDRESS_ERROR =
  '地址格式无效，请输入 IP 或域名，可附加 :端口（1–65535），默认端口 29875；IPv6 带端口时使用 [地址]:端口';

function validIpv4(host: string): boolean {
  const parts = host.split('.');
  return (
    parts.length === 4 &&
    parts.every((part) => /^(0|[1-9][0-9]{0,2})$/.test(part) && Number(part) <= 255)
  );
}

function validIpv6(host: string): boolean {
  let value = host;
  if (value.includes('.')) {
    const i = value.lastIndexOf(':');
    if (!validIpv4(value.slice(i + 1))) return false;
    value = value.slice(0, i + 1) + '0:0';
  }
  const halves = value.split('::');
  if (halves.length > 2) return false;
  const groups = halves.flatMap((half) => (half ? half.split(':') : []));
  return (
    groups.every((group) => /^[0-9a-fA-F]{1,4}$/.test(group)) &&
    (halves.length === 2 ? groups.length < 8 : groups.length === 8)
  );
}

export function validHost(host: string): boolean {
  if (host.includes(':')) return validIpv6(host);
  if (/^[0-9.]+$/.test(host) && host.includes('.')) return validIpv4(host);
  const name = host.endsWith('.') ? host.slice(0, -1) : host;
  return (
    name.length > 0 &&
    name.length <= 253 &&
    name.split('.').every((label) => /^[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?$/.test(label))
  );
}

/** IPv6 with an explicit port must be bracketed; bare IPv6 uses the default port. */
export function parseAddr(raw: string): { host: string; port: number } | null {
  const value = raw.trim();
  let host = value;
  let portText: string | undefined;
  if (value.startsWith('[')) {
    const match = /^\[([^\]]+)\](?::(.*))?$/.exec(value);
    if (!match || !validIpv6(match[1]!)) return null;
    host = match[1]!;
    portText = match[2];
  } else if (value.includes(':') && value.indexOf(':') === value.lastIndexOf(':')) {
    [host, portText] = value.split(':') as [string, string];
  }
  if (!validHost(host)) return null;
  if (portText !== undefined && !/^[0-9]{1,5}$/.test(portText)) return null;
  const port = portText === undefined ? DEFAULT_PORT : Number(portText);
  return port >= 1 && port <= 65535 ? { host, port } : null;
}

export function formatAddr(host: string, port: number): string {
  return `${host.includes(':') ? `[${host}]` : host}:${port}`;
}
