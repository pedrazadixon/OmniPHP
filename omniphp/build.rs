//! Embeds the application icon in the executable.
//!
//! It goes in as icon resource 1, which is where three things look for it:
//! Windows Explorer (the exe's own icon), GPUI (the window's taskbar icon) and
//! the tray icon code in `ui::tray`.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut resources = winresource::WindowsResource::new();
        resources.set_icon_with_id("assets/icon.ico", "1");
        resources.set("ProductName", "OmniPHP");
        resources.set("FileDescription", "OmniPHP");
        if let Err(error) = resources.compile() {
            // Without the Windows SDK's resource compiler the app still builds,
            // just with the default icon.
            println!("cargo:warning=application icon not embedded: {error}");
        }
    }
}
