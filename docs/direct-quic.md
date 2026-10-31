# 直连 QUIC

直连连接页可选择混合 TCP/UDP、纯 TCP 或 QUIC。QUIC 使用输入地址的现有 UDP 端口，不需要配置额外端口；连接失败会报错，可手动切换其他协议。VNT 通道仍由服务器节点入口使用，独立承担中继和 NAT 打洞。

QUIC 使用 TLS 1.3，ALPN 为 `vnt-direct-quic/1`，禁用 0-RTT。访问密码经 Argon2id v1.3（19456 KiB、2 次迭代、并行度 1、32 字节输出，盐为 `vnt-desk/direct-quic/tls-key/v1`）派生 Ed25519 密钥。空白密码使用内部默认值 `vnt-desk/direct-quic/passwordless/v1`。客户端校验派生公钥与实际握手签名，应用层使用密码挑战认证。密钥和证书仅在内存中生成和缓存，密码修改作用于后续握手。

控制和文件使用各自的 QUIC 可靠流。音视频使用 DATAGRAM，承载帧分片、多屏路由、FEC 与限时修复协议；FEC 使用连接级开关。媒体适配层保留 12 字节分片头（大端 `u64 sequence + u16 index + u16 count`），根据 QUIC 的 DATAGRAM 上限分片，兼容最大音频包和 FEC 包。分片重组限制为 64 片、64 KiB、128 个在途包和 150 ms，并在连接更换时清空。

服务端只有一个物理 UDP 接收入口。有效的原生 UDP 绑定包经会话、凭证和来源校验后交给混合模式，其他报文交由 Quinn 处理。QUIC 媒体按连接关联，不使用原生 UDP 绑定，也不按 IP 合并会话。恢复可靠流和整条 QUIC 连接时沿用现有 30 秒恢复期限；服务停止时关闭连接并释放端口。

混合模式在主控端使用同一个媒体 UDP socket，每 10 秒重发带有会话 ID 和绑定凭证的 `UdpBind`，维持 NAT 映射及状态防火墙记录。保活独立于音视频接收，会话结束时停止；初次收流前进行绑定重试。QUIC 使用协议层 ACK 和 2 秒空闲保活，VNT 使用其自身传输机制。

验证命令：`cargo test -p vnt-net`、`cargo test -p vnt-host`、`cargo test -p vnt-proto`、`cargo check -p vnt-remote`、`npm test` 和 `npm run build`。
