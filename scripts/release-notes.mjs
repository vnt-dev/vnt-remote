import { execFileSync } from 'node:child_process';
import process from 'node:process';
import { pathToFileURL } from 'node:url';

export function appendDownloadTable(release) {
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
  const table = [
    '## 系统版本与下载',
    '',
    '| 系统版本 | 架构 | 类型 | 下载链接 |',
    '| --- | --- | --- | --- |',
    ...rows,
  ].join('\n');
  const body = release.body ?? '';
  if (body.includes(table)) return body;
  return `${body}${body ? '\n\n' : ''}${table}\n`;
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
  const body = appendDownloadTable(release);
  if (body === (release.body ?? '')) return;
  // Send only body: preserve the title and all other release metadata.
  execFileSync(
    'gh',
    ['api', '--method', 'PATCH', `repos/${repository}/releases/${release.databaseId}`, '--input', '-'],
    {
      input: JSON.stringify({ body }),
      stdio: ['pipe', 'ignore', 'inherit'],
    },
  );
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main();
