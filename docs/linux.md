# Linux 使用与构建

首版面向 Ubuntu 22.04 及以上的 x86_64 **X11 / Xorg 桌面会话**，提供 deb 与 AppImage。两端请使用相同版本。

## 支持的功能

Linux 可作为控制端和被控端，支持多屏、单屏区域共享、键鼠控制、文字与 PNG 图片剪贴板，以及系统声音采集和远程声音播放。

被控端使用 H.264 软件编码。控制端通过 WebKitGTK / GStreamer 探测 H.264、HEVC、AV1 的实际解码能力，再与被控端协商；具体能力取决于本机解码插件与驱动。

首版不支持 Wayland、文件与文件夹剪贴板、Linux 硬件编码、修改显示器物理分辨率、被控提示边框、登录前远控和服务开机启动。deb 的安装不创建系统服务。锁屏时暂停画面和输入；解锁后重新确认捕获源。无法确认会话锁定状态时保持暂停。

## 安装与启动

在发布页下载 `_amd64.deb` 或 `_amd64.AppImage`。

```bash
sudo apt install ./vnt-remote_*_amd64.deb
```

或者给 AppImage 可执行权限后运行：

```bash
chmod +x ./vnt-remote_*_amd64.AppImage
./vnt-remote_*_amd64.AppImage
```

AppImage 如果提示缺少 FUSE，可使用发行版的 FUSE 兼容包，或通过 `APPIMAGE_EXTRACT_AND_RUN=1 ./vnt-remote_*_amd64.AppImage` 解压运行。AppImage 包含媒体框架，但仍需要正常的桌面会话、音频服务和系统密钥环。

在 Ubuntu 登录界面选择用户后，通过齿轮菜单选择 **Ubuntu on Xorg**，再登录。仅设置 `GDK_BACKEND=x11` 不会把 Wayland 会话变成 X11；通过 XWayland 启动同样不属于支持范围。

首次连接步骤见 [README](../README.md#第一次连接)。无需用 root 运行程序。

## 声音、剪贴板与密码

- 声音使用 PulseAudio，或 PipeWire 提供的 `pipewire-pulse` 服务。共享的是默认音频输出的 monitor，不采集麦克风。切换系统默认输出后会重建采集流；播放设备丢失时回退到默认设备，界面显示原因。
- 文字和图片使用标准 CLIPBOARD 剪贴板。鼠标中键使用的 PRIMARY selection 不参与同步。某些终端的粘贴快捷键为 Ctrl+Shift+V，可手动粘贴同步后的文本。
- 本机访问密码与 VNT 配置中的订阅、加密密钥由 Secret Service 兼容密钥环保管，例如 GNOME Keyring。密钥环需要随用户会话启动并解锁；不能访问时保存会报错并保留原配置，不会写入明文。
- 配置与日志位于 Tauri 用户应用数据目录，通常为 `${XDG_DATA_HOME:-$HOME/.local/share}/com.vnt.remote/`；配置在其 `portable/config.json` 中。这里的 `portable` 表示用户会话模式，deb 也使用此目录。
- 键盘输入在远程画面获得焦点时转发。Alt+Tab、桌面切换等系统保留快捷键仍可能由本机处理。Command、Alt、Control 的物理键位沿用既有协议映射。

关闭主窗口后应用驻留托盘；从托盘选择「退出」可停止共享和连接。Linux 首版请从发布页下载新版本，彻底退出后替换 AppImage，或安装新的 deb。

## 从源码构建

构建基线为 Ubuntu 22.04 x86_64；在更高版本系统构建的二进制可能要求更高版本的 glibc。发布工作流固定在 Ubuntu 22.04 构建。

安装 Rust stable、Node.js 24，然后在仓库根目录执行：

```bash
bash scripts/setup-linux.sh
npm ci
npm run tauri -- dev
```

构建两个发布包：

```bash
npm run build:linux
```

输出目录为 `target/release/bundle/deb/` 与 `target/release/bundle/appimage/`。不要为 Linux 启用 `installed` feature，该 feature 专用于 Windows 服务版。

控制端要求 WebKitGTK 2.44 或以上与可用的 GStreamer 解码插件；deb 声明相关依赖，AppImage 启用媒体框架打包。参考 [Tauri Linux 依赖](https://v2.tauri.app/start/prerequisites/#linux)、[AppImage 构建说明](https://v2.tauri.app/distribute/appimage/)和 [WebKitGTK WebCodecs 支持](https://webkitgtk.org/2024/03/27/webkigit-2.44.html)。

## 验证

```bash
npm run lint
npm run format:check
npm test
npm run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --exclude vnt-remote --locked
cargo test -p vnt-remote --lib --locked
bash scripts/test-linux-desktop.sh
# 先构建发布包，再验证两个包内的真实远程画面
npm run build:linux
bash scripts/test-linux-packages.sh
```

桌面测试脚本创建独立 Xvfb、D-Bus 与 PulseAudio 虚拟输出，验证真实 XTEST 事件、抓屏像素、剪贴板 INCR 传输与音频 monitor，不改动正在使用的桌面剪贴板或默认音频输出。

发布前仍需在 Ubuntu 22.04、24.04 的真实 Xorg 桌面逐项验收 deb 和 AppImage：

- Linux 与 Linux、Windows、macOS 双向连接；实际远程画面、键鼠、文字图片和声音均正常。
- 多屏、区域选择、拖动、缩放、断线重连，以及失焦后按键释放。
- 锁屏暂停、解锁恢复；屏幕断开或显示模式变化后暂停并重新确认来源。
- 音频默认输出切换、显式设备移除、音频服务中断后的状态和恢复。
- Wayland / 无显示会话错误提示、密钥环未解锁的保存失败、托盘退出。

自动化测试或单次编译不能替代这些跨平台实机验收。

## 常见问题

| 现象                 | 检查方式                                                                                                           |
| -------------------- | ------------------------------------------------------------------------------------------------------------------ |
| 提示仅支持 X11       | 退出登录并选择 Xorg 会话；检查 `echo "$XDG_SESSION_TYPE"`，同时确保没有继承 Wayland 会话变量                       |
| 连接时没有可用解码器 | 更新 WebKitGTK，安装 `gstreamer1.0-libav`、`gstreamer1.0-plugins-good` 与 `gstreamer1.0-plugins-bad`，然后重启应用 |
| 密码无法保存或读取   | 确认 Secret Service 密钥环已启动并解锁；不要删除仍引用旧密钥的配置文件                                             |
| 没有系统声音         | 检查 `pactl info` 是否连接正常、默认输出是否正确，以及本机音量和连接中的音频开关                                   |
| 解锁后仍暂停         | 确认桌面提供 GNOME / freedesktop ScreenSaver 或 logind 会话状态，再重新开启被控                                    |
| 看不到托盘图标       | 确认桌面提供 AppIndicator 支持；Ubuntu GNOME 通常通过系统自带扩展提供                                              |
