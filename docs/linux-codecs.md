# Linux 视频编码

Ubuntu 22.04+ x86_64 / X11 支持下列编码路径。输入为 BGRA，经 BT.709 转换为 8-bit 4:2:0；H.264 / HEVC 输出 Annex B，AV1 输出低开销 OBU。

| 格式  | Intel / AMD | NVIDIA | 软件兜底 |
| ----- | ----------- | ------ | -------- |
| H.264 | VA-API      | NVENC  | OpenH264 |
| HEVC  | VA-API      | NVENC  | Kvazaar  |
| AV1   | VA-API      | NVENC  | SVT-AV1  |

硬件开关表示「优先硬编，不可用时使用同格式软件编码」。开启后按 NVENC 设备索引、VA-API render 节点名称排序尝试。每种格式必须通过真实初始化、编码及关键帧码流检查才会公布能力；连接时还会用实际尺寸、帧率、码率预检。自动协商排序保持不变，新增软件 AV1 不会越过可用硬编。

初始化、重配或运行故障会丢弃失败输出并回退同格式软件编码，发送流重置与新关键帧，更新两端的硬件标记与后端名称。该流随后保持软件编码；重连或重建流时再尝试硬件。软件编码尤其 AV1 可能需要更多 CPU。控制端同时检查 WebCodecs 能力查询和真实关键帧解码输出，未安装相应解码器或自测失败时，在协商前禁用该格式并显示原因。

## 驱动与设备权限

安装包提供编码接口库与私有 VA-API 加载库，不提供显卡驱动。

- Intel / AMD：安装适用于显卡的 VA-API 驱动，例如 Ubuntu 的 `intel-media-va-driver` 或 `mesa-va-drivers`。用户需能读写 `/dev/dri/renderD*`；检查 `id` 与 `ls -l /dev/dri/renderD*`，必要时由管理员授予发行版推荐的 `render` 组权限并重新登录。
- NVIDIA：安装支持 NVENC 的 NVIDIA 驱动。固定使用的 12.0 头文件要求 Linux 驱动至少 520.56.06；新显卡或 AV1 可能要求更高版本。显卡型号、驱动和会话权限均影响实际支持。
- AV1 硬编需要显卡本身具备 AV1 编码单元。能解码 AV1 不代表能编码。

不要用 root 启动桌面应用。无设备、无权限、驱动不兼容或模块缺失时仍可使用三种软件编码。

WebKitGTK 的部分版本在 AV1 能力查询成功后仍无法解码，因为没有插入 `av1parse`；见 [上游缺陷 327481](https://bugs.webkit.org/show_bug.cgi?id=327481)。请使用包含该修复的 WebKitGTK 与相应 GStreamer 插件。本机 2.52.3 可复现此缺陷，因此 AV1 的 GStreamer 软件回读通过，但本机安装包 WebCodecs 应拒绝 AV1；不能将此项计为 AV1 实际播放通过。

## 构建、加载与源码

先执行 `bash scripts/setup-linux.sh` 安装工具，再正常执行 Cargo 或 `npm run build:linux`。构建脚本下载并校验 [sources.json](../crates/vnt-codec/linux/sources.json) 中固定的源码：FFmpeg 6.1.6、nv-codec-headers 12.0.16.1、libva 2.20.0。首次构建需要联网；下载缓存位于 `target/linux-codec/downloads/`。

FFmpeg 只启用六个硬编编码器及必要组件，未启用 GPL / nonfree。静态 FFmpeg 封装在 `libvnt-linux-encoder.so` 中，隐藏内部符号；VA-API 使用独立 SONAME。应用通过窄 C ABI 动态加载该可替换模块，与 WebKitGTK / GStreamer 隔离。Kvazaar、SVT-AV1 沿用仓库源码并静态链接；Linux 构建验证 SVT 的 C / Rust 结构布局。

明确的加载路径：

- Cargo 开发运行、`codec-probe`：构建时生成的 `target/linux-codec/runtime/libvnt-linux-encoder.so`，可用绝对路径环境变量 `VNT_LINUX_ENCODER_LIB` 覆盖。
- Tauri 开发运行：如有资源目录中的模块则使用该路径，否则使用 Cargo 构建路径。
- deb / AppImage：只使用应用资源目录内的 `linux-codec/libvnt-linux-encoder.so`；加载失败会保留软件能力。

模块旁包含私有 `libva-vnt.so.2`、`libva-drm-vnt.so.2`，系统提供 libdrm 与 GPU 驱动。安装包包含相关许可证。发布附件 `vnt-linux-codec-sources.tar.xz` 包含精确源码归档、校验清单、C ABI 源码、导出表和构建配置；项目修改仅为独立封装与构建配方，未修改上游源码。

重建模块：将源码包中的 `vnt-linux-codec/` 文件放回仓库 `crates/vnt-codec/linux/`，三个上游归档放入 `target/linux-codec/downloads/`，执行 `python3 crates/vnt-codec/linux/build.py`。替换安装资源中的整个 `linux-codec/` 目录即可使用重建模块；请保留 ABI 版本和相邻运行库。构建路径改变或修改 FFmpeg / libva 配置后，先删除 `target/linux-codec/` 下除 `downloads/` 外的构建产物。分发依据 [FFmpeg 官方许可说明](https://ffmpeg.org/legal.html)。

## 自测与验收

```bash
# 设备、各格式/后端的真实探测结果与失败原因
cargo run -p vnt-codec --bin codec-probe
# 关闭硬编，编码 60 帧
cargo run -p vnt-codec --bin codec-probe -- test hevc 1280x720 soft
# 严格硬编；不能通过软件回退算成功
cargo run -p vnt-codec --bin codec-probe -- test av1 1280x720 --hardware-only
# 三种软件编码及 GStreamer 解码回读
bash scripts/test-linux-codecs.sh
# 在已准备的 GPU 机器上执行，参数为该显卡应支持的格式
bash scripts/test-linux-gpu.sh h264 hevc av1
```

普通 Linux CI 覆盖软件回读、关键帧、时间戳、重配、并发、退出、可注入硬编故障与元数据同步、TCP / 混合 / QUIC 传输，以及安装包真实 WebCodecs 播放。GPU 脚本保留设备与后端日志，且要求实际 `硬件=true`。不支持的格式应作为该显卡能力限制记录，不可改用软件算硬编通过。

本次开发环境初始没有 render 访问权限；后续桌面会话获得设备 ACL 后，Intel UHD Graphics 770（8086:a780）的 VA-API H.264 / HEVC 已通过严格硬编、60 帧解码回读、关键帧、时间戳、尺寸 / 帧率 / 码率重配与退出测试。该设备 AV1 硬编返回不支持，严格模式正确失败。**AMD、NVIDIA，以及 AV1 硬编正向实机验收未执行**。Ubuntu 22.04 / 24.04、多屏、区域与动态尺寸、跨 Windows / macOS 互通仍须结合真实 GPU 和桌面逐项验收。
