import { describe, expect, it } from 'vitest';
import { DEFAULT_PORT, formatAddr, parseAddr } from './address';

describe('device addresses', () => {
  it.each([
    '192.168.1.10',
    'example.com',
    'my-pc',
    'localhost',
    'example.com.',
    '::1',
    '2001:db8::1',
    '::ffff:192.168.1.10',
  ])('uses the default port for %s', (host) => {
    expect(parseAddr(host)).toEqual({ host, port: DEFAULT_PORT });
    expect(parseAddr(`  ${host}  `)).toEqual({ host, port: DEFAULT_PORT });
    expect(parseAddr(formatAddr(host, 31000))).toEqual({ host, port: 31000 });
  });
  it.each([
    '192.168.1.10:',
    'example.com:',
    ':29875',
    'host:0',
    'host:65536',
    'host:-1',
    'host:+80',
    'host:1.5',
    'host:abc',
    'host: 80',
    'host:000080',
    '256.1.1.1',
    '01.2.3.4',
    '127.1',
    'https://example.com',
    'host/path',
    'user@host',
    'host name',
    '-host',
    'host-',
    'a..b',
    '',
    '[::1]:',
    '[::1]:0',
    '[host]:80',
    '1::2::3',
    '::ffff:192.168.1.999',
  ])('rejects malformed address %j', (value) => expect(parseAddr(value)).toBeNull());
  it('accepts port boundaries and bracketed IPv6', () => {
    expect(parseAddr('example.com:1')).toEqual({ host: 'example.com', port: 1 });
    expect(parseAddr('192.168.1.10:65535')).toEqual({ host: '192.168.1.10', port: 65535 });
    expect(parseAddr('[::1]')).toEqual({ host: '::1', port: DEFAULT_PORT });
    expect(parseAddr('a'.repeat(64) + '.com')).toBeNull();
    expect(parseAddr(Array(5).fill('a'.repeat(63)).join('.'))).toBeNull();
  });
});
