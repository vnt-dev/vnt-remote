//! 拥塞控制：关键帧节流、接收端反馈驱动的码率/帧率自适应。
//!
//! 要防的死循环：拥塞丢帧 → 每个丢包都请求关键帧 → 大关键帧突发发送 →
//! 更拥塞。旧代码在发送队列每次打满时都置关键帧标志，持续拥塞下等于
//! 每帧都发 IDR（数倍于 P 帧），链路永远排不空。
//!
//! - [`SendCongestion`]：结合接收丢帧、RTT 和发送侧压力，先降码率、持续
//!   拥塞后降帧率；稳定后缓慢恢复。
//!
//! 其他组件：
//! - [`KeyframeGate`]：所有关键帧请求（发送队列打满、客户端
//!   `RequestKeyframe`）统一走一个闸门，至多每 [`KEYFRAME_MIN_INTERVAL`]
//!   满足一次；等待节流窗口时仍继续编码发送，由需要恢复的接收端等待 IDR。

mod adaptive;
pub use adaptive::{FeedbackInbox, SendCongestion};

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 关键帧最小间隔。拥塞下 IDR 至多每秒两个：再密就会反过来加重拥塞
/// （原始逻辑每个丢包都触发一个，持续拥塞时等于每帧都发 IDR）。
/// 正常视频持续发送，重复的刷新请求不能让健康接收端也冻结。
pub const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// 客户端重复请求关键帧、且 IDR 已欠这么久，才算"接收端受害"证据。
/// 取节流间隔（1s）+ 500ms 余量：正常调度下 IDR 不该欠这么久。
const RECEIVER_DISTRESS_AFTER: Duration = Duration::from_millis(1500);

/// 关键帧闸门：跨线程共享（会话 reader 与管线线程）。
/// 内部互斥锁从不在调用方代码执行期间持有。
pub struct KeyframeGate {
    state: Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    /// 有未满足的关键帧需求（丢帧后重建参考链 / 客户端请求）。
    wanted: bool,
    /// 紧急需求：不受节流间隔限制（首帧、分辨率变化——客户端要凭 IDR
    /// 重配解码器，晚一个间隔就是一两秒花屏）。
    urgent: bool,
    /// `wanted` 由 false 转 true 的时刻（判断接收端是否等太久）。
    wanted_since: Option<Instant>,
    /// 上次关键帧实际发出的时刻（节流时钟）。
    last_emitted: Option<Instant>,
}

impl KeyframeGate {
    /// 新闸门：首帧即关键帧。
    pub fn new() -> Self {
        Self {
            state: Mutex::new(GateState {
                wanted: true,
                urgent: true,
                ..Default::default()
            }),
        }
    }

    /// 记录一次关键帧请求（客户端请求 / 丢帧后重建参考链）。
    /// 返回 true 表示这是"IDR 已欠很久仍未发出"的重复请求——接收端长
    /// 时间解不了码，属于拥塞/丢包证据，调用方可据此降码率。
    pub fn request(&self, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap();
        let distress = state.wanted
            && state
                .wanted_since
                .is_some_and(|since| now.duration_since(since) >= RECEIVER_DISTRESS_AFTER);
        if !state.wanted {
            state.wanted = true;
            state.wanted_since = Some(now);
        }
        distress
    }

    /// 紧急请求（分辨率变化）：立即满足，绕过节流间隔。
    pub fn request_urgent(&self) {
        let mut state = self.state.lock().unwrap();
        state.wanted = true;
        state.urgent = true;
        state.wanted_since.get_or_insert_with(Instant::now);
    }

    /// 是否欠着关键帧；不代表现有参考帧失效。
    #[cfg(test)]
    pub fn wanted(&self) -> bool {
        self.state.lock().unwrap().wanted
    }

    /// 现在是否允许发关键帧（节流窗口已过，或紧急需求）。
    pub fn due(&self, now: Instant) -> bool {
        let state = self.state.lock().unwrap();
        state.wanted
            && (state.urgent
                || state
                    .last_emitted
                    .is_none_or(|t| now.duration_since(t) >= KEYFRAME_MIN_INTERVAL))
    }

    /// 关键帧已实际入队：清除需求、重置节流时钟。
    pub fn mark_emitted(&self, now: Instant) {
        let mut state = self.state.lock().unwrap();
        state.wanted = false;
        state.urgent = false;
        state.wanted_since = None;
        state.last_emitted = Some(now);
    }
}

impl Default for KeyframeGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_frame_is_due_immediately() {
        let gate = KeyframeGate::new();
        assert!(gate.wanted(), "首帧必须关键帧");
        assert!(gate.due(t0()));
    }

    #[test]
    fn request_is_throttled_within_interval() {
        let gate = KeyframeGate::new();
        let start = t0();
        gate.mark_emitted(start);
        assert!(!gate.wanted());
        // 间隔内的请求：记下需求但不放行
        assert!(!gate.request(start + Duration::from_millis(100)));
        assert!(gate.wanted());
        assert!(!gate.due(start + Duration::from_millis(100)));
        // 窗口过后放行
        assert!(gate.due(start + KEYFRAME_MIN_INTERVAL));
    }

    #[test]
    fn repeated_requests_coalesce_into_one_keyframe() {
        let gate = KeyframeGate::new();
        let start = t0();
        gate.mark_emitted(start);
        for i in 1..10 {
            gate.request(start + Duration::from_millis(i * 10));
        }
        // 十个请求只应产生一个待发关键帧
        assert!(gate.wanted());
        assert!(gate.due(start + KEYFRAME_MIN_INTERVAL));
        gate.mark_emitted(start + KEYFRAME_MIN_INTERVAL);
        assert!(!gate.wanted(), "发出后需求必须清除，否则每帧都是 IDR");
    }

    #[test]
    fn urgent_request_bypasses_throttle() {
        let gate = KeyframeGate::new();
        let start = t0();
        gate.mark_emitted(start);
        gate.request_urgent();
        // 刚发过 IDR，普通请求会被节流；紧急（分辨率变化）必须立即放行
        assert!(gate.due(start + Duration::from_millis(1)));
        gate.mark_emitted(start + Duration::from_millis(1));
        assert!(!gate.wanted());
        assert!(!gate.due(start + Duration::from_millis(2)));
    }

    #[test]
    fn distress_only_after_long_outstanding_idr() {
        let gate = KeyframeGate::new();
        let start = t0();
        gate.mark_emitted(start);
        // 欠了 IDR，但还在节流窗口内：不算受害
        let asked_at = start + Duration::from_millis(200);
        gate.request(asked_at);
        assert!(!gate.request(asked_at + Duration::from_millis(200)));
        // 从"欠着 IDR"起算超过阈值：接收端真的解不了，记为受害
        assert!(gate.request(asked_at + RECEIVER_DISTRESS_AFTER));
        // 没有欠着 IDR 时的重复请求不算受害（客户端可能只是话多）
        gate.mark_emitted(asked_at + RECEIVER_DISTRESS_AFTER);
        assert!(!gate.request(asked_at + RECEIVER_DISTRESS_AFTER + Duration::from_millis(1)));
        assert!(!gate.request(asked_at + RECEIVER_DISTRESS_AFTER + Duration::from_millis(2)));
    }
}
