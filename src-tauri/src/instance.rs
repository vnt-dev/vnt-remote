//! One desktop per edition and Windows session, including startup handoff.
use std::sync::Arc;
use vnt_host::windows::native::{wide, Handle};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0},
        System::Threading::{CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject},
    },
};

pub struct Instance {
    _mutex: Handle,
    show: Arc<Handle>,
}

impl Instance {
    pub fn acquire(installed: bool) -> anyhow::Result<Option<Self>> {
        Self::named(if installed { "installed" } else { "portable" })
    }

    fn named(edition: &str) -> anyhow::Result<Option<Self>> {
        let name = format!(r"Local\VntRemote.Desktop.{edition}");
        unsafe {
            // Create the event first so a second launch can notify us even
            // while the first process is still preparing Tauri or the service.
            let show = Handle(CreateEventW(
                None,
                false,
                false,
                PCWSTR(wide(format!("{name}.Show")).as_ptr()),
            )?);
            let mutex = Handle(CreateMutexW(None, false, PCWSTR(wide(name).as_ptr()))?);
            if GetLastError() == ERROR_ALREADY_EXISTS {
                SetEvent(show.0)?;
                return Ok(None);
            }
            Ok(Some(Self {
                _mutex: mutex,
                show: Arc::new(show),
            }))
        }
    }

    pub fn listen(&self, app: tauri::AppHandle) {
        let show = self.show.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                if unsafe { WaitForSingleObject(show.0, 0) } == WAIT_OBJECT_0 {
                    crate::tray::show_main_window(&app);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_launch_notifies_owner_and_exit_allows_relaunch() {
        let name = format!("test.{}", vnt_host::windows::native::random().unwrap());
        let first = Instance::named(&name).unwrap().unwrap();
        assert!(Instance::named(&name).unwrap().is_none());
        assert_eq!(
            unsafe { WaitForSingleObject(first.show.0, 0) },
            WAIT_OBJECT_0
        );
        drop(first);
        assert!(Instance::named(&name).unwrap().is_some());
    }

    #[test]
    fn editions_can_run_together() {
        let name = vnt_host::windows::native::random().unwrap();
        let installed = Instance::named(&format!("{name}.installed"))
            .unwrap()
            .unwrap();
        let portable = Instance::named(&format!("{name}.portable"))
            .unwrap()
            .unwrap();
        assert!(Instance::named(&format!("{name}.installed"))
            .unwrap()
            .is_none());
        assert!(Instance::named(&format!("{name}.portable"))
            .unwrap()
            .is_none());
        drop((installed, portable));
    }

    #[test]
    fn second_process_restores_existing_instance() {
        const TEST_NAME: &str = "VNT_INSTANCE_TEST_NAME";
        if let Ok(name) = std::env::var(TEST_NAME) {
            assert!(Instance::named(&name).unwrap().is_none());
            return;
        }
        let name = format!("test.{}", vnt_host::windows::native::random().unwrap());
        let first = Instance::named(&name).unwrap().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "instance::tests::second_process_restores_existing_instance",
                "--nocapture",
            ])
            .env(TEST_NAME, name)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        assert_eq!(
            unsafe { WaitForSingleObject(first.show.0, 0) },
            WAIT_OBJECT_0
        );
    }
}
