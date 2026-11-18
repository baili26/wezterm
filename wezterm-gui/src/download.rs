use mux::zmodem::sink;

pub fn save_to_downloads(orig_name: Option<String>, data: &[u8]) -> anyhow::Result<()> {
    let dir = sink::default_download_dir()?;
    let name = sink::write_file(&dir, orig_name.as_deref(), data, false)?;

    let url = format!("file://{}", name.display());
    wezterm_toast_notification::persistent_toast_notification_with_click_to_open_url(
        "Download completed",
        &format!("Downloaded {}", name.display()),
        &url,
    );

    log::info!("Downloaded {}", name.display());

    Ok(())
}
