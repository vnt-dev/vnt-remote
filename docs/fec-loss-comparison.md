# FEC 开关验证

使用当前代码分别开启和关闭 FEC，对比视频恢复、音频连续性、流量、CPU 和内存。只对比相同构建及相同条件，结果与具体编码器、码率和网络模型相关。

## 测试方法

- 使用真实回环 TCP/UDP、中继丢包及合成画面/声音；不采集麦克风，不写入用户文件。
- 视频验证先预热自适应状态，再采样实际桥接帧率、目标帧率/码率、原始丢包、恢复帧及残余丢失；TCP 控制通道不注入丢包。
- 随机丢包对原始包和校验包使用相同规则，固定种子保证可重复；突发场景按固定窗口连续丢包。不额外模拟 RTT、限带宽或抖动。
- 音频使用合成 Opus 帧，统计恢复、PLC、过期包、缓冲和连续性；结果不包含扬声器物理播放质量。
- 无损对比使用 `fec_lossless_comparison` 记录编码、封装及校验带来的 CPU、内存和额外流量。

## 运行

在具备可用编码器的环境中串行执行；部分应用集成路径需要 Windows 交互桌面。输出保存在 `target/`。

```powershell
cargo test -p vnt-remote --test fec_comparison -- --ignored --nocapture --test-threads=1
cargo test -p vnt-remote --test fec_audio_comparison -- --ignored --nocapture --test-threads=1
cargo test -p vnt-remote --test performance fec_lossless_comparison -- --ignored --nocapture --test-threads=1

$env:FEC_COMPARE_SEED='2402370172'
$env:FEC_COMPARE_CASES='random_1pct,burst_3_of_100'
$env:FEC_COMPARE_SETTLE_SECONDS='10'
$env:FEC_COMPARE_SECONDS='12'
$env:FEC_COMPARE_OUT='target/fec-comparison.json'
cargo test -p vnt-remote --test fec_comparison -- --ignored --nocapture --test-threads=1

$env:FEC_AUDIO_COMPARE_CASES='random_3pct,random_10pct'
$env:FEC_AUDIO_COMPARE_SETTLE_SECONDS='8'
$env:FEC_AUDIO_COMPARE_SECONDS='8'
$env:FEC_AUDIO_COMPARE_OUT='target/fec-audio-comparison.json'
cargo test -p vnt-remote --test fec_audio_comparison -- --ignored --nocapture --test-threads=1
```

报告需包含当前构建、硬件、分辨率、编码器、预热/采样时长和随机种子。短时回环不能证明真实广域网性能；FEC 增加冗余，全部包丢失时也无法恢复。多次重复、改变种子，并单独验收实际解码播放。
