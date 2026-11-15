#![cfg(target_os = "linux")]
#[test]
#[ignore = "requires an isolated Secret Service with an unlocked default collection"]
fn secret_service_persists_without_plaintext_and_preserves_file_on_failure() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    let config = vnt_host::config::AppConfig {
        password: "native-secret-service-password".into(),
        ..Default::default()
    };
    vnt_host::store::save_at(&path, &config).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(&config.password));
    assert_eq!(
        vnt_host::store::load_from(&path).unwrap().password,
        config.password
    );
    // Lose the bus deliberately after a successful commit. This is a separate
    // test process, so the environment change cannot affect other binaries.
    let previous = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
    std::env::set_var(
        "DBUS_SESSION_BUS_ADDRESS",
        "unix:path=/nonexistent/vnt-test-bus",
    );
    let result = vnt_host::store::save_at(
        &path,
        &vnt_host::config::AppConfig {
            password: "new-value".into(),
            ..config.clone()
        },
    );
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", previous);
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(
        vnt_host::store::load_from(&path).unwrap().password,
        config.password
    );
    // Delete the stored secret after the test.
    vnt_host::store::save_at(&path, &Default::default()).unwrap();
}
