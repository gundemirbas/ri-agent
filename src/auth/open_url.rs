/// Try to open `url` in the user's default browser using `xdg-open`.
/// Returns an error if the helper cannot be launched; the caller
/// decides how to surface that to the user.
pub fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}
