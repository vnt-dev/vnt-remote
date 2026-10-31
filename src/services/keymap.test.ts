import { describe, expect, it } from 'vitest';
import { lookupCode } from './keymap';

describe('Windows keyboard scan codes', () => {
  // Expected Set-1 values are independent of the lookup implementation.
  it.each([
    ['PrintScreen', 0x37, true],
    ['ContextMenu', 0x5d, true],
    ['NumpadEqual', 0x59, false],
    ['IntlBackslash', 0x56, false],
    ['F13', 0x64, false],
    ['F14', 0x65, false],
    ['F15', 0x66, false],
    ['F16', 0x67, false],
    ['F17', 0x68, false],
    ['F18', 0x69, false],
    ['F19', 0x6a, false],
    ['F20', 0x6b, false],
    ['F21', 0x6c, false],
    ['F22', 0x6d, false],
    ['F23', 0x6e, false],
    ['F24', 0x76, false],
    ['MediaTrackPrevious', 0x10, true],
    ['MediaTrackNext', 0x19, true],
    ['MediaPlayPause', 0x22, true],
    ['MediaStop', 0x24, true],
    ['AudioVolumeMute', 0x20, true],
    ['AudioVolumeDown', 0x2e, true],
    ['AudioVolumeUp', 0x30, true],
  ] as const)('maps %s without losing its E0 prefix', (code, scancode, extended) => {
    expect(lookupCode(code)).toEqual({ scancode, extended });
  });

  it('distinguishes extended keys from ordinary keys sharing the same scan code', () => {
    expect(lookupCode('PrintScreen')).not.toEqual(lookupCode('NumpadMultiply'));
    expect(lookupCode('MediaTrackPrevious')).not.toEqual(lookupCode('KeyQ'));
    expect(lookupCode('Enter')).not.toEqual(lookupCode('NumpadEnter'));
  });

  it.each(['Pause', 'Unidentified', '', 'toString', '__proto__'])(
    'rejects unsupported %s',
    (code) => {
      expect(lookupCode(code)).toBeUndefined();
    },
  );
});
