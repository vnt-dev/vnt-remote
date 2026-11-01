# Release 与应用内更新

公开仓库及默认更新源为 `vnt-dev/vnt-remote`。普通 push 不运行 Checks，仅 PR 运行校验；推送 `v*` 标签或手动运行 Release 会构建 Windows x64 NSIS 安装包、免安装 EXE 和 macOS arm64 免安装 app.zip。

## 首次配置

使用 Tauri updater 签名私钥、公钥及私钥密码。私钥与密码保存在仓库外的受保护位置；对应公钥写入 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`。

在运行工作流的 GitHub 仓库 Settings → Secrets and variables → Actions 中添加：

- Secret `TAURI_SIGNING_PRIVATE_KEY`：私钥文件的完整内容。
- Secret `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：密码文件内容。
- 可选 Variable `RELEASE_REPOSITORY`：发布目标，默认 `vnt-dev/vnt-remote`。工作流会同步生成对应更新地址。
- 可选 Variable `ANDROID_RELEASE_REPOSITORY`：安卓发布仓库，默认 `vnt-dev/vnt-remote-app`。生成下载表格时查询与桌面端完全相同的版本标签（例如 `v0.1.0`），仅在该 Release 已发布且包含 APK 附件时加入安卓下载链接；找不到对应 Release、仍为草稿或没有 APK 时跳过安卓。查询遇到其他错误时会报错，便于发现权限或网络问题。
- 如果从私有源码仓库向公开仓库发布，还需 Secret `RELEASE_TOKEN`，授予目标仓库 Contents 读写权限。在公开仓库自身发布可使用自带 `GITHUB_TOKEN`。
- 跨仓库发布时，目标仓库需已有 `main` 分支；其他分支可用 Variable `RELEASE_COMMITISH` 指定。安装包仍从源码仓库指定版本标签构建。

私钥和密码不要提交到 Git。请备份并保留同一套密钥；更换公钥后，旧安装程序不能验证新密钥签名的更新。这些是更新包签名密钥，不是 Windows Authenticode 证书。

## 发布

1. 将根目录 `package.json`、`Cargo.toml` 的 workspace 版本与 `src-tauri/tauri.conf.json` 版本改成同一个值；同步 npm/Cargo 锁文件中本项目的版本。
2. 提交后创建并推送相同版本标签，首版为 `v0.1.0`。手动运行 Release 时也必须填写已存在的版本标签。
3. 工作流验证版本一致，构建并签名，创建 **Draft Release**，上传 Windows 安装包、`.sig`、`latest.json`，以及 Windows 与 macOS 免安装产物。
4. 两个平台上传成功后，工作流自动在现有 Release notes 末尾追加系统版本、架构、安装类型与实际附件下载链接的表格，并在安卓同标签 Release 已发布时加入其 APK 下载链接。重跑时更新已有下载表格，可以补入稍后发布的安卓附件。上传和更新说明均保留已有 Release title 及表格之外的说明。
5. 检查产物后在 GitHub 发布该草稿。稳定版发布后，已安装客户端即可从设置页检查更新。预发布版本不作为 latest 稳定更新源。

本地 `npm run build:installed` 可构建未签名安装包，无需 updater 私钥。需要本地签名时设置上述两个 `TAURI_SIGNING_*` 环境变量、`RELEASE_TAG`，运行 `node scripts/prepare-release.mjs`，再运行 `npm run tauri -- build --config src-tauri/tauri.installed.conf.json --config src-tauri/tauri.release.conf.json -- --locked`。

## 应用内更新

设置页提供检查更新、下载更新、安装更新。下载完成并通过签名验证后才显示安装按钮。失败可重试；没有 Content-Length 时显示不定进度条与已下载字节数。

更新对象与进度保存在模块级状态中。页面切换不取消下载，侧栏持续显示下载进度和待安装状态，返回设置后继续展示。关闭整个应用会结束本次下载，不支持跨重启断点续传。

安装会退出主界面和控制窗口，由 NSIS 提权并执行服务升级钩子。升级本机被控服务可能中断连接。更新资源必须可匿名 HTTPS 访问，客户端不内置 GitHub Token。

端到端验收需要先发布一个签名的新版本，再从较旧的已安装版本依次检查、下载、切换页面观察进度、安装并确认应用版本及被控服务状态。

## macOS 免安装发布

发布流程先创建统一草稿 Release，Windows 与 macOS 作业随后上传各自产物。macOS 使用 `macos-15` Apple Silicon runner，运行 `npm run build:portable`，仅上传 `*-macos-arm64-portable.app.zip`，不参与 Windows 的 `latest.json` 自动更新产物。macOS 不要求 Tauri updater 签名密钥，默认 ad-hoc 本地签名；首次运行、安全限制及双机验收见 [macOS 文档](macos.md)。主控与被控须使用相同的协议 v1。
