import { execFileSync } from 'node:child_process';
import process from 'node:process';
import { pathToFileURL } from 'node:url';

export function appendDownloadTable(release, androidRelease) {
  const platforms = [
    ['Windows', 'x64', '安装版', /_x64-setup\.exe$/i],
    ['Windows', 'x64', '免安装版', /-windows-x64-portable\.exe$/i],
    ['macOS 14+', 'Apple Silicon (arm64)', '免安装版', /-macos-arm64-portable\.app\.zip$/i],
  ];
  const rows = platforms.map(([system, arch, edition, pattern]) => {
    const matches = release.assets.filter((asset) => pattern.test(asset.name));
    if (matches.length !== 1) throw new Error(`Expected one ${system} ${edition} asset`);
    return `| ${system} | ${arch} | ${edition} | [下载](${matches[0].url}) |`;
  });
  if (androidRelease && !androidRelease.draft && androidRelease.published_at) {
    for (const asset of androidRelease.assets) {
      const match = /^VNTRemote-(arm64-v8a|x86_64)-release\.apk$/i.exec(asset.name);
      if (!match) continue;
      const arch = match[1].toLowerCase();
      rows.push(`| Android 7.0+ | ${arch} | APK | [下载](${asset.browser_download_url}) |`);
    }
  }
  const table = [
    '## 系统版本与下载',
    '',
    '| 系统版本 | 架构 | 类型 | 下载链接 |',
    '| --- | --- | --- | --- |',
    ...rows,
  ].join('\n');
  const body = release.body ?? '';
  // Replace the generated table so a rerun can pick up a later Android release.
  const existingTable =
    /^## 系统版本与下载\r?\n\r?\n\| 系统版本 \| 架构 \| 类型 \| 下载链接 \|\r?\n\| --- \| --- \| --- \| --- \|(?:\r?\n\|[^\r\n]*\|)*/m;
  if (existingTable.test(body)) return body.replace(existingTable, () => table);
  return `${body}${body ? '\n\n' : ''}${table}\n`;
}

export function findAndroidRelease(repository, tag) {
  try {
    return JSON.parse(
      execFileSync('gh', ['api', `repos/${repository}/releases/tags/${encodeURIComponent(tag)}`], {
        encoding: 'utf8',
        stdio: ['ignore', 'pipe', 'pipe'],
      }),
    );
  } catch (error) {
    if (/\(HTTP 404\)/.test(error.stderr?.toString() ?? '')) {
      process.stdout.write(
        `No Android release for ${repository}@${tag}; skipping Android downloads.\n`,
      );
      return null;
    }
    throw error;
  }
}

function main() {
  const repository = process.env.RELEASE_REPOSITORY;
  const tag = process.env.RELEASE_TAG;
  if (!repository || !tag) throw new Error('RELEASE_REPOSITORY and RELEASE_TAG are required');
  const release = JSON.parse(
    execFileSync(
      'gh',
      ['release', 'view', tag, '--repo', repository, '--json', 'databaseId,body,assets'],
      { encoding: 'utf8' },
    ),
  );
  const androidRepository = process.env.ANDROID_RELEASE_REPOSITORY || 'vnt-dev/vnt-remote-app';
  const androidRelease = findAndroidRelease(androidRepository, tag);
  const body = appendDownloadTable(release, androidRelease);
  if (body === (release.body ?? '')) return;
  // Send only body: preserve the title and all other release metadata.
  execFileSync(
    'gh',
    [
      'api',
      '--method',
      'PATCH',
      `repos/${repository}/releases/${release.databaseId}`,
      '--input',
      '-',
    ],
    {
      input: JSON.stringify({ body }),
      stdio: ['pipe', 'ignore', 'inherit'],
    },
  );
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main();
