use serde::Serialize;
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformInfo {
    platform: &'static str,
    file_clipboard: bool,
    physical_resolution: bool,
    screen_recording: bool,
    accessibility: bool,
    hardware_encode: bool,
    capture_border: bool,
    desktop_available: bool,
    desktop_unavailable_reason: String,
}
#[tauri::command]
pub fn platform_info() -> Result<PlatformInfo, String> {
    #[cfg(target_os = "linux")]
    {
        let error = vnt_linux::session::check_x11().err();
        Ok(PlatformInfo {
            platform: "linux",
            file_clipboard: false,
            physical_resolution: false,
            screen_recording: error.is_none(),
            accessibility: error.is_none(),
            hardware_encode: true,
            capture_border: false,
            desktop_available: error.is_none(),
            desktop_unavailable_reason: error.unwrap_or_default(),
        })
    }

    #[cfg(target_os = "macos")]
    {
        let permission = vnt_macos::query(1)?;
        Ok(PlatformInfo {
            hardware_encode: true,
            capture_border: false,
            desktop_available: true,
            desktop_unavailable_reason: String::new(),
            platform: "macos",
            file_clipboard: false,
            physical_resolution: false,
            screen_recording: permission["screenRecording"] == true,
            accessibility: permission["accessibility"] == true,
        })
    }
    #[cfg(windows)]
    {
        Ok(PlatformInfo {
            hardware_encode: true,
            capture_border: true,
            desktop_available: true,
            desktop_unavailable_reason: String::new(),
            platform: "windows",
            file_clipboard: true,
            physical_resolution: true,
            screen_recording: true,
            accessibility: true,
        })
    }
}
pub fn check_host_permissions(control: bool) -> Result<(), String> {
    let info = platform_info()?;
    if !info.desktop_available {
        return Err(info.desktop_unavailable_reason);
    }
    validate_host_permissions(info.screen_recording, info.accessibility, control)
}
fn validate_host_permissions(
    screen: bool,
    accessibility: bool,
    control: bool,
) -> Result<(), String> {
    if !screen {
        return Err("请在系统设置 → 隐私与安全性中授予屏幕录制权限，然后重新打开应用".into());
    }
    if control && !accessibility {
        return Err("请授予辅助功能权限，或关闭「允许控制」以观看模式启动".into());
    }
    Ok(())
}

#[tauri::command]
pub async fn request_platform_permission(permission: String, settings: bool) -> Result<(), String> {
    if !matches!(permission.as_str(), "screenRecording" | "accessibility") {
        return Err("未知权限".into());
    }
    #[cfg(target_os = "macos")]
    {
        // Permission prompts may wait for the user. Keep the UI event loop running.
        tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
            if settings {
                let pane = if permission == "screenRecording" {
                    "ScreenCapture"
                } else {
                    "Accessibility"
                };
                vnt_macos::json(
                    3,
                    &serde_json::json!(format!(
                        "x-apple.systempreferences:com.apple.preference.security?Privacy_{pane}"
                    )),
                )?;
            } else {
                vnt_macos::json(2, &serde_json::json!(permission))?;
            }
            Ok(())
        })
        .await
        .map_err(|error| error.to_string())??;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = settings;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_capabilities_never_advertise_windows_only_features() {
        let info = platform_info().unwrap();
        assert_eq!(info.platform, "linux");
        assert!(!info.file_clipboard && !info.physical_resolution && !info.capture_border);
        assert_eq!(
            info.desktop_available,
            info.desktop_unavailable_reason.is_empty()
        );
    }
    #[test]
    fn host_permissions_fail_closed_and_allow_explicit_view_only() {
        assert!(validate_host_permissions(false, true, false).is_err());
        assert!(validate_host_permissions(false, false, true).is_err());
        assert!(validate_host_permissions(true, false, true).is_err());
        assert!(validate_host_permissions(true, false, false).is_ok());
        assert!(validate_host_permissions(true, true, true).is_ok());
    }
}
