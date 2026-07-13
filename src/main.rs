mod anthropic;
mod config;
mod desktop;
#[cfg(target_os = "macos")]
mod macos_tray;
mod proxy;
mod responses_api;

fn main() -> eframe::Result<()> {
    set_macos_app_icon();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([980.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "recodexProxyHub",
        options,
        Box::new(|cc| Ok(Box::new(desktop::HubApp::new(cc)))),
    )
}

#[cfg(target_os = "macos")]
fn set_macos_app_icon() {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSString};

    let Some(icon_path) = bundled_icon_path() else {
        return;
    };
    let Some(icon_path) = icon_path.to_str() else {
        return;
    };
    let mtm = unsafe { MainThreadMarker::new_unchecked() };

    let path = NSString::from_str(icon_path);
    let Some(image): Option<Retained<NSImage>> =
        (unsafe { NSImage::initWithContentsOfFile(mtm.alloc(), &path) })
    else {
        return;
    };

    let app = NSApplication::sharedApplication(mtm);
    unsafe {
        app.setApplicationIconImage(Some(&image));
    }
}

#[cfg(target_os = "macos")]
fn bundled_icon_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    let contents_dir = macos_dir.parent()?;
    Some(contents_dir.join("Resources").join("AppIcon.icns"))
}

#[cfg(not(target_os = "macos"))]
fn set_macos_app_icon() {}
