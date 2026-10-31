pub fn pick() -> Result<Option<vnt_capture::ScreenRegion>, String> {
    let value = vnt_macos::query(11)?;
    if value.is_null() {
        return Ok(None);
    }
    let mut region: vnt_capture::ScreenRegion =
        serde_json::from_value(value).map_err(|e| e.to_string())?;
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).map_err(|e| e.to_string())?;
    region.id = id.iter().map(|b| format!("{b:02x}")).collect();
    region.validate()?;
    Ok(Some(region))
}
