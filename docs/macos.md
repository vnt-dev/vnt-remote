# macOS 免安装版

支持 macOS 14.0 及以上的 Apple Silicon（arm64）Mac。Mac 可作为主控或被控，与 Windows / Mac 互控；两端均须使用协议 v1 的版本。

## 下载与运行

下载 `vnt-remote-<version>-macos-arm64-portable.app.zip`，解压后双击 `vnt-remote.app` 即可运行，无需安装器或系统服务。可将应用保留在自己的固定目录；不要在压缩包内运行。首次打开可能需要在「系统设置 → 隐私与安全性」中允许打开这个应用。

当前使用 ad-hoc 本地签名，未经过 Apple Developer ID 签名或公证。Gatekeeper 可能阻止来自浏览器的下载；确认发布来源后，按系统提供的「仍要打开」流程操作。如果系统没有提供允许入口，不要关闭系统全局安全保护。正式分发可通过 Tauri 的 `APPLE_SIGNING_IDENTITY` 和公证环境变量配置正式证书；本地构建不需要这些凭据。

关闭主窗口会驻留菜单栏托盘，当前远程会话继续运行。通过托盘菜单或 Dock 图标恢复主窗口；选择「退出」或使用应用退出菜单将断开连接并停止本机被控。更新时先退出，再下载新压缩包替换 `.app`。

## 首次授权

首页显示系统权限状态，只在用户点击授权按钮时请求权限：

- **屏幕录制**：被控端共享屏幕、区域和系统播放声音所需。缺少权限时不会启动被控。授权后按系统提示退出并重新打开应用。
- **辅助功能**：允许远程设备注入键盘和鼠标所需。没有权限时可主动关闭「允许控制」，以观看模式启动。

纯主控连接不要求屏幕录制权限。键盘接管只在当前获得焦点的远程画面内生效；离开画面、失焦、关闭窗口或断线会释放已按下的按键。macOS 保留的系统快捷键可能由本机处理。键位保持物理语义：Meta 对应 Command，Alt 对应 Option，Control 对应 Control；不自动将 Ctrl+C 改为 Command+C。

移动应用或替换版本后，本地签名发生变化可能使系统要求重新授权；请从系统设置中确认当前这份应用的权限。

### 系统设置已开启，但应用仍提示未授权

ad-hoc 签名的应用身份绑定到具体构建。修改代码并重新打包后，旧构建的授权可能不再适用于当前程序，即使系统设置里的开关仍然开启。

1. 从托盘菜单彻底退出 vnt-remote。
2. 在「系统设置 → 隐私与安全性 → 屏幕与系统音频录制」中选中旧的 vnt-remote，点击「−」移除。
3. 点击「+」，使用 Command+Shift+G 定位到当前项目下的 `target/debug/bundle/macos/`，添加其中的 `vnt-remote.app` 并开启授权。
4. 运行 `npm run start:macos` 打开已有应用。按系统提示退出并重新打开一次。若辅助功能也出现同样现象，对其旧条目执行相同步骤。

仅关闭再启动同一份应用不会改变代码身份；每次修改后的重新构建才可能需要重新授权。跨构建保留授权需要用同一有效证书签名，例如开发时设置 `APPLE_SIGNING_IDENTITY` 为已安装的 Apple Development 证书。参见 [Apple 对代码身份与授权的说明](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements)。

点击请求后没有弹窗时，打开「系统设置 → 隐私与安全性 → 屏幕与系统音频录制」。如果列表中没有 vnt-remote，点击「+」选择当前运行的 `vnt-remote.app`。授权后彻底退出应用再重新打开（关闭窗口只会驻留托盘）。

## 功能边界

支持多屏和区域共享、屏幕拆分窗口、TCP / TCP+UDP / QUIC / VNT、H.264 / HEVC 硬件编码、H.264 软件兜底、文本/图片双向剪贴板和系统音频。主控按 WebKit 实际能力探测解码格式，H.264 是互通基线；没有可用 HEVC 解码器时不会协商 HEVC。Mac 被控端不提供 AV1 编码。

涉及 Mac 的连接不支持文件或文件夹剪贴板，界面明确提示；协议能力协商会阻止 Windows 向 Mac 发送文件清单和文件请求。Mac 不修改物理显示模式，支持会话内视频缩放；收到物理分辨率修改请求时回退软件缩放并返回原因。

系统声音由 ScreenCaptureKit 采集，不采集麦克风，并排除本应用播放的声音。主控通过 CoreAudio 播放，支持设备选择与不可用时回退默认设备。双方互控时仍建议至少关闭一个方向的音频，避免其他应用形成回环。

macOS 被控只在当前登录且解锁的用户会话中工作，不提供开机服务、登录界面或锁屏操作。锁屏/退出当前控制台会话时暂停共享，恢复后重建采集。受 DRM 保护的画面或声音可能无法采集。

配置位于 Tauri 应用数据目录下的 `portable/config.json`，密码存放在 Keychain 的 `com.vnt.remote.host` 服务项中，JSON 仅保存引用。Keychain 无法访问或配置损坏会返回错误，不回退为明文保存。日志位于相同应用数据目录的 `logs/`。

## 本地构建

需要 Apple Silicon Mac、Xcode Command Line Tools、Rust stable，以及 Node 22.13+（22.x）或 24+。Opus 使用上游 portable 源码清单经系统 C 编译器静态构建，macOS 不要求安装 CMake；OpenH264 随 Rust 依赖静态构建。

```sh
npm ci
npm run dev:macos
npm run build:portable
```

`dev:macos` 构建并打开 `target/debug/bundle/macos/vnt-remote.app`，包含应用标识、权限说明和本地签名，用于测试系统权限；每次修改后重新运行即可。启动前退出旧实例。仅构建不启动可使用 `npm run dev:macos -- --build-only`。

日常重新打开已构建的应用使用 `npm run start:macos`；此命令不会重新构建或签名。

`npm run tauri dev` 仍可用于界面热更新，但它直接运行 `target/debug/vnt-remote`，没有完整的 `.app` 身份；系统可能无法将屏幕录制权限关联到 vnt-remote，权限测试请使用上述应用包。

产物位于 `target/release/bundle/portable/`。只生成 `.app.zip`，不生成 DMG、PKG 或自动更新安装包。构建脚本校验签名后使用 `ditto --keepParent --sequesterRsrc` 打包，保留应用结构和元数据。

## 验证

普通检查不采集真实屏幕、不注入输入、不修改系统剪贴板；涉及真实桌面的测试必须显式运行。

```sh
npm run lint
npm run format:check
npm test
npm run build
MACOSX_DEPLOYMENT_TARGET=14.0 cargo clippy --workspace --all-targets --locked -- -D warnings
MACOSX_DEPLOYMENT_TARGET=14.0 cargo test --workspace --exclude vnt-remote --locked
MACOSX_DEPLOYMENT_TARGET=14.0 cargo test -p vnt-remote --lib --locked

# 交互桌面：会真实抓屏，在内存中编码，不保存画面
cargo test -p vnt-macos native_capture_and_encoding -- --ignored --nocapture --test-threads=1
cargo test -p vnt-capture --test desktop_idle -- --ignored --test-threads=1
```

会话状态的原生回归测试使用模拟系统字典，不请求权限或抓屏：

```sh
clang -fobjc-arc -fblocks -mmacosx-version-min=14.0 -Wno-unused-parameter \
  crates/vnt-macos/tests/session_state.m -o target/debug/macos-session-state-test \
  -framework AppKit -framework ApplicationServices -framework ScreenCaptureKit \
  -framework VideoToolbox -framework AVFoundation -framework AudioToolbox \
  -framework Security -framework CoreMedia -framework CoreVideo -framework CoreAudio
target/debug/macos-session-state-test
```

发布前人工验收 Mac→Windows、Windows→Mac、Mac→Mac：TCP/UDP、双屏和 Retina/非 Retina 混用、区域框选、窗口拆分、Shift/Command/Option、拖拽和滚轮、文本/透明 PNG、系统声音及音频设备拔插。覆盖拒绝/撤销权限、锁屏、睡眠唤醒、显示器拔插、断线重连和退出。真实剪贴板、输入和音频测试只在明确准备好的交互桌面上执行。

最终需从浏览器下载发布压缩包，检查首次打开、授权及实际双机连接；本地编译、合成流和签名校验不能代替该项验收。
