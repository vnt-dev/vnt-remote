import { describe, expect, it } from 'vitest';
import type { ProfileStatus } from './api';
import {
  connectionIdLabel,
  linkPhaseLabel,
  profileConnectionError,
  profileNameError,
  syncDeviceGroups,
} from './linkProfiles';
const status = (configId: string, name: string, enabled = true): ProfileStatus => ({
  configId,
  name,
  enabled,
  phase: 'connected',
  error: null,
  networkCode: 'same-group',
  deviceId: 'local',
  localConnectionId: '001',
  devices: [{ id: 'same-node', name: '设备', connectionId: '0001', direct: true }],
});
describe('server profile names', () => {
  const configs = [
    { id: 'a', name: '办公室' },
    { id: 'b', name: '家里' },
  ];
  it('requires a nonblank name and enforces the same 64-character limit as the backend', () => {
    expect(profileNameError({ id: 'new', name: '  ' }, configs)).toContain('请填写');
    expect(profileNameError({ id: 'new', name: '名'.repeat(65) }, configs)).toContain('64');
    expect(profileNameError({ id: 'new', name: '😀'.repeat(64) }, configs)).toBe('');
  });
  it('rejects new and edited duplicates after trimming and permits editing itself', () => {
    expect(profileNameError({ id: 'new', name: ' 办公室 ' }, configs)).toBe('配置名称已存在');
    expect(profileNameError({ id: 'b', name: '办公室' }, configs)).toBe('配置名称已存在');
    expect(profileNameError({ id: 'a', name: ' 办公室 ' }, configs)).toBe('');
  });
});
describe('synced device groups', () => {
  it('keeps profile order and duplicate nodes and IPs, excluding stopped profiles', () => {
    const groups = syncDeviceGroups([
      status('b', '家里'),
      status('a', '办公室'),
      status('c', '停止', false),
    ]);
    expect(groups.map((g) => g.label)).toEqual(['同组设备-家里', '同组设备-办公室']);
    expect(groups.map((g) => [g.configId, g.devices[0].id, g.devices[0].connectionId])).toEqual([
      ['b', 'same-node', '0001'],
      ['a', 'same-node', '0001'],
    ]);
  });
  it('retains empty connecting, reconnecting and failed groups with status labels', () => {
    for (const phase of ['connecting', 'reconnecting', 'error']) {
      const group = syncDeviceGroups([{ ...status('a', '办公室'), phase, devices: [] }])[0];
      expect(group.devices).toEqual([]);
      expect(linkPhaseLabel(group.phase)).not.toBe('已停止');
    }
  });
});

describe('required connection fields', () => {
  const manual = {
    mode: 'manual',
    subscription: '',
    servers: ['tcp://127.0.0.1:29872'],
    networkCode: 'group',
    connectionId: '123456789',
  };
  it('requires all manual fields and rejects whitespace', () => {
    expect(profileConnectionError(manual)).toBe('');
    expect(profileConnectionError({ ...manual, servers: ['  '] })).toBe('请填写服务器地址');
    expect(profileConnectionError({ ...manual, networkCode: ' ' })).toBe('请填写房间编号');
    expect(profileConnectionError({ ...manual, connectionId: ' ' })).toBe('请填写连接 ID');
  });
  it('requires a subscription address independently of manual fields', () => {
    const config = {
      ...manual,
      mode: 'subscription',
      servers: [],
      networkCode: '',
      connectionId: '',
    };
    expect(profileConnectionError(config)).toBe('请填写订阅地址');
    expect(profileConnectionError({ ...config, subscription: ' ' })).toBe('请填写订阅地址');
    expect(profileConnectionError({ ...config, subscription: 'vnt2://join/2/example' })).toBe('');
  });
  it('accepts one to nine ASCII digits including zero-only identifiers', () => {
    for (const connectionId of [
      '0',
      '00',
      '001',
      '0001',
      '000000000',
      '12345678',
      '123456789',
      '999999999',
    ])
      expect(profileConnectionError({ ...manual, connectionId })).toBe('');
    for (const connectionId of ['1234567890', '12345678a', '１２３４５６７８９', '7.91.205.21'])
      expect(profileConnectionError({ ...manual, connectionId })).not.toBe('');
  });
  it('displays connection identifiers without interpreting them as IP addresses', () => {
    expect(connectionIdLabel('001')).toBe('001');
    expect(connectionIdLabel('0001')).toBe('0001');
    expect(connectionIdLabel('')).toBe('');
    expect(connectionIdLabel('1.2.3.256')).toBe('');
  });
});
