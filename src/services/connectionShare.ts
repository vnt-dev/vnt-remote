import { parseAddr } from './address';

export interface SharedConnection {
  address: string;
  password: string;
}

/** A clipboard format only; pasting it never opens a URL or starts a connection. */
export function formatConnectionShare(address: string, password: string): string {
  const query = new URLSearchParams({ address, password });
  return `vntremote://connect?${query}`;
}

/** Leave ordinary address pastes to the input's native behavior. */
export function parseConnectionShare(text: string): SharedConnection | null {
  const trimmed = text.trim();
  if (!/^vntremote:/i.test(trimmed)) return null;
  const invalid = () => new Error('地址和密码格式无效，请重新复制');
  let url: URL;
  try {
    url = new URL(trimmed);
  } catch {
    throw invalid();
  }
  const address = url.searchParams.get('address');
  const password = url.searchParams.get('password');
  if (
    url.hostname !== 'connect' ||
    url.username ||
    url.password ||
    url.port ||
    (url.pathname !== '' && url.pathname !== '/') ||
    url.hash ||
    url.searchParams.getAll('address').length !== 1 ||
    url.searchParams.getAll('password').length !== 1 ||
    !address ||
    password === null
  ) {
    throw invalid();
  }
  if (!parseAddr(address)) throw invalid();
  return { address, password };
}
