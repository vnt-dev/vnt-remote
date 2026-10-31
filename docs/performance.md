# 视频性能验证

## 当前管线

采集源将 `Arc<CapturedFrame>` 直接发布到各订阅。每个订阅只保留一个未读帧，覆盖计入原始帧替换；按帧率过滤不计丢帧。采集按最高订阅帧率运行，最后一个订阅取消时中断等待。发布不等待编码或网络，慢会话不会阻塞其他会话。

原生生产者最多保留一个已发布帧作为复用候选，只有独占所有权时才复用像素分配；读取者仍持有时另行分配。采集结束释放候选帧。每个编码器保留必要的像素转换/缩放缓冲，媒体队列、重组窗口、重传缓存和恢复快照均有界。

背压位于编码前；已编码队列保持参考链顺序。TCP 媒体生产与控制/文件通道独立，UDP 在发送容量可用时推进。网络序号在实际开始发送时分配，部分发送消耗序号，本地尚未发送的原始帧替换不算网络丢失。发送以绝对字节期限节奏推进，限制空闲信用。

关键帧请求受节流约束，节流期间继续编码普通参考帧。解码器恢复同时考虑实际等待时间和媒体时间跨度，单纯的时间戳跳跃不等于解码器卡死。多屏各自有帧率与码率预算，总带宽应按所有已打开流计算。

TCP 输出等待可参与拥塞判断；UDP 容量等待包含正常节奏控制，其质量调整依赖接收端反馈。原始帧替换、编码耗时和网络丢包分别统计，不能互相替代。

## 自动检查

```sh
cargo test -p vnt-proto -p vnt-net
cargo test -p vnt-capture --lib
cargo test -p vnt-host --lib
cargo test -p vnt-codec --lib
cargo test -p vnt-remote --lib
npm test
npm run build
npm run lint
```

针对性验证：

```sh
cargo test --release -p vnt-host --lib keyframe_refresh_preserves_single_screen_throughput -- --nocapture --test-threads=1
cargo test -p vnt-host --lib multiscreen --locked
cargo test -p vnt-net --lib multiscreen --locked
npm test -- useRemoteDecoder
cargo test -p vnt-remote --release --test adaptive_link -- --nocapture --test-threads=1
```

弱网中继测试验证丢包后的降码率/帧率及同一会话恢复。单元测试覆盖发送节奏、关键帧突发、部分发送取消、序号回绕、参考依赖和媒体背压下的独立控制消息。

## 交互与长时间测试

硬件编码、真实抓屏与进程私有内存测量主要在 Windows 交互桌面执行。一次只运行一个编码/捕获基准，保持桌面解锁和相同的移动内容，关闭其他占用编码器的测试或构建。

```powershell
# 自带移动棋盘窗口，测量真实主屏采集与硬件 H.264
cargo run --release -p vnt-codec --example motion_encode -- 300

# 确定性 4K、真实编解码与回环传输
cargo test -p vnt-remote --release --test performance synthetic_4k_tcp_and_hybrid_keep_comparable_throughput -- --ignored --exact --nocapture --test-threads=1

# 真实桌面 TCP / 混合模式
cargo test -p vnt-remote --release --test performance desktop_tcp_and_hybrid_keep_comparable_throughput -- --ignored --exact --nocapture --test-threads=1

# 十次连接/断开，观察会话清理与内存回落
cargo test -p vnt-remote --release --test performance ten_reconnections_release_each_session -- --ignored --exact --nocapture --test-threads=1

# 多编码器、多码率的采集 → 编码 → 传输矩阵
$env:BENCH_OUT='target/throughput-matrix.txt'
cargo test -p vnt-remote --release --test performance throughput_matrix -- --ignored --exact --nocapture --test-threads=1
```

4K TCP / 混合模式各预热 5 秒、采样 60 秒，以 30 FPS / 8000 kbps 比较桥接吞吐；混合模式要求达到 TCP 的 90%。十次连接交替使用 4K 与 1080p 缩放，断开后等待驱动清理再采样内存，最终三次采样应回到预热后低点的 64 MiB 范围内。

`throughput_matrix` 使用确定性来源，各项预热 3 秒、采样 6 秒，按 100 ms 采样同一进程内服务端、主控及视频桥的私有内存。记录在窗口内的中位值与断开两秒后的空闲值。低码率行会主动触及发送预算，帧率下降不应直接归因于采集或编码故障。

## 原生 HEVC 运动测试

`native_hevc_motion_matrix` 使用主屏与自带移动窗口，经硬件 HEVC、真实 TCP/混合传输交付至本地 WebSocket 桥。

```powershell
$env:HEVC_BENCH_FPS='30,60'
$env:HEVC_BENCH_KBPS='16000'
$env:HEVC_BENCH_SECONDS='45'
$env:BENCH_OUT='target/hevc-motion.txt'
cargo test --release -p vnt-remote --test performance native_hevc_motion_matrix -- --ignored --exact --nocapture --test-threads=1
```

记录实际桥接 FPS、最小一秒 FPS、帧间隔 p95/最大值、停顿、已应用参数及私有内存。`relative_jitter_p95` 是接收时间与 PTS 偏移减去最小值后的变化，只测变化的队列延迟，不是绝对端到端延迟。报告硬件、分辨率、刷新率和具体构建，重复运行后评估差异。

## 实机验收与解释

桥接帧率不包含 WebView 解码、绘制或显示延迟；私有内存不包含 WebView 进程树。需单独检查原生应用显示 FPS、解码延迟和应用加 WebView 的总内存。抓屏与编码可并行，不能将平均耗时直接相加估算 FPS。

Windows 服务代理测试覆盖持久管道、代次与配置/关键帧顺序、暂停时心跳、取消和过期输入拒绝；实际登录、锁屏/UAC、用户切换及多屏拔插仍需 [Windows 验收](windows-service.md)。Mac 的原生验证见 [macOS 说明](macos.md)。

多屏验收覆盖不同 DPI、负显示坐标、区域重叠、主控或本机鼠标跨屏、拆分与最小化、关闭初始屏幕后音频/剪贴板继续工作。FEC 的无损开销与丢包对比见 [FEC 验证](fec-loss-comparison.md)。性能结果只对记录的构建与条件负责，不将旧二进制或不同机器的测量值作为当前代码结论。
