#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod anthropic;
mod auth_quota;
mod config;
mod config_import;
mod desktop;
mod google_ai;
#[cfg(target_os = "macos")]
mod macos_tray;
mod oauth_login;
mod proxy;
mod responses_api;
mod web;

fn main() -> eframe::Result<()> {
    let window_title = format!("RouteHub v{}", app_version());

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 760.0])
        .with_min_inner_size([980.0, 640.0]);
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        &window_title,
        options,
        Box::new(|cc| Ok(Box::new(desktop::HubApp::new(cc)))),
    )
}

pub(crate) fn app_version() -> &'static str {
    option_env!("ROUTEHUB_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

// 通过 eframe 的 ViewportBuilder 设置窗口/Dock 图标。
//
// 早期版本在 main() 里直接调用 NSApplication::sharedApplication 提前触发 AppKit
// 初始化，在 macOS 15 上会因为 `_NSInitializeAppContext` 查询菜单栏状态时
// `abort()` 而闪退（表现为 DMG 安装后双击没反应）。winit 有专门的时序处理，
// 交给 with_icon 让 winit 在合适时机设置图标。
#[cfg(target_os = "macos")]
fn load_window_icon() -> Option<egui::IconData> {
    let bytes: &[u8] =
        include_bytes!("../icon/AppIcons/Assets.xcassets/AppIcon.appiconset/256.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

#[cfg(not(target_os = "macos"))]
fn load_window_icon() -> Option<egui::IconData> {
    None
}
