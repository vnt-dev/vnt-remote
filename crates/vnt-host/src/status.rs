use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub installed: bool,
    pub state: String,
    #[serde(default)]
    pub service_state: String,
    pub auto_start: bool,
    pub running: bool,
    pub port: u16,
    pub sessions: Vec<crate::server::SessionInfo>,
    pub console_session: Option<u32>,
    pub desktop: String,
    pub error: Option<String>,
    pub monitors: Vec<vnt_capture::MonitorInfo>,
    pub password_configured: bool,
}
impl Status {
    pub fn validate_port_change(&self, requested_port: u16) -> Result<(), String> {
        if (self.running || matches!(self.state.as_str(), "running" | "starting" | "stopping"))
            && requested_port != self.port
        {
            return Err("服务启动后不能修改端口，请先停止被控服务".into());
        }
        Ok(())
    }
}
