import { describe, expect, it } from 'vitest';
import { formatConnectionShare, parseConnectionShare } from './connectionShare';

describe('connection clipboard sharing', () => {
  it.each(['simple-password', ' 空 格 &+=?#%:/\\中文🔑 ', '', 'line1\r\nline2'])(
    'preserves the password exactly: %j',
    (password) => {
      const text = formatConnectionShare('192.168.1.100:29875', password);
      expect(text).not.toMatch(/[\r\n]/);
      expect(parseConnectionShare(`\r\n${text}\r\n`)).toEqual({
        address: '192.168.1.100:29875',
        password,
      });
    },
  );

  it.each(['192.168.1.100', '192.168.1.100:29875', 'my-pc:31000', 'https://example.com'])(
    'does not intercept ordinary pasted text: %s',
    (text) => expect(parseConnectionShare(text)).toBeNull(),
  );

  it.each([
    'vntremote:broken',
    'vntremote://other?address=host:29875&password=secret',
    'vntremote://connect?address=host:29875',
    'vntremote://connect?address=host:29875&password=a&password=b',
    'vntremote://connect?address=host:29875&address=other:29875&password=a',
    'vntremote://user@connect?address=host:29875&password=a',
    'vntremote://connect/path?address=host:29875&password=a',
    'vntremote://connect?address=host:29875&password=a#fragment',
    formatConnectionShare('host:0', 'secret'),
    formatConnectionShare('host:65536', 'secret'),
    formatConnectionShare('host name:29875', 'secret'),
  ])('rejects invalid shares without returning credentials: %s', (text) => {
    expect(() => parseConnectionShare(text)).toThrow('地址和密码格式无效');
  });
});

it.each(['192.168.1.10', 'example.com', 'example.com:31000', '[::1]:31000'])(
  'accepts all supported shared address formats: %s',
  (address) => {
    expect(parseConnectionShare(formatConnectionShare(address, 'pw'))).toEqual({
      address,
      password: 'pw',
    });
  },
);
