fn main() {
    // the Windows exe icon (File Explorer, taskbar, shortcuts)
    #[cfg(windows)]
    embed_resource::compile("icon.rc", embed_resource::NONE).manifest_optional().ok();
}
