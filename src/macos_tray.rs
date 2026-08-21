use eframe::egui;
use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use std::sync::mpsc::{self, Receiver};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    ShowWindow,
    ToggleServer,
    Quit,
}

pub struct MacosTray {
    _tray_icon: TrayIcon,
    show_item: MenuItem,
    toggle_item: MenuItem,
    quit_item: MenuItem,
    status_item: MenuItem,
    events: Receiver<MenuEvent>,
    running: bool,
}

impl MacosTray {
    pub fn new(ctx: &egui::Context) -> Result<Self, String> {
        let show_item = MenuItem::new("显示主窗口", true, None);
        let status_item = MenuItem::new("代理状态：已停止", false, None);
        let toggle_item = MenuItem::new("启动代理", true, None);
        let quit_item = MenuItem::new("退出 RouteHub", true, None);
        let separator = PredefinedMenuItem::separator();
        let menu = Menu::new();
        menu.append_items(&[
            &show_item,
            &status_item,
            &separator,
            &toggle_item,
            &PredefinedMenuItem::separator(),
            &quit_item,
        ])
        .map_err(|err| format!("创建状态栏菜单失败: {err}"))?;

        let icon = load_icon()?;
        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("RouteHub - 代理已停止")
            .with_icon(icon)
            .build()
            .map_err(|err| format!("创建状态栏图标失败: {err}"))?;

        let (sender, events) = mpsc::channel();
        let repaint_ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = sender.send(event);
            repaint_ctx.request_repaint();
        }));

        Ok(Self {
            _tray_icon: tray_icon,
            show_item,
            toggle_item,
            quit_item,
            status_item,
            events,
            running: false,
        })
    }

    pub fn next_action(&self) -> Option<TrayAction> {
        while let Ok(event) = self.events.try_recv() {
            if event.id() == self.show_item.id() {
                return Some(TrayAction::ShowWindow);
            }
            if event.id() == self.toggle_item.id() {
                return Some(TrayAction::ToggleServer);
            }
            if event.id() == self.quit_item.id() {
                return Some(TrayAction::Quit);
            }
        }
        None
    }

    pub fn set_running(&mut self, running: bool) {
        if self.running == running {
            return;
        }
        self.running = running;
        if running {
            self.status_item.set_text("代理状态：运行中");
            self.toggle_item.set_text("停止代理");
            let _ = self._tray_icon.set_tooltip(Some("RouteHub - 代理运行中"));
        } else {
            self.status_item.set_text("代理状态：已停止");
            self.toggle_item.set_text("启动代理");
            let _ = self._tray_icon.set_tooltip(Some("RouteHub - 代理已停止"));
        }
    }
}

fn load_icon() -> Result<Icon, String> {
    let image = image::load_from_memory(include_bytes!(
        "../icon/AppIcons/Assets.xcassets/AppIcon.appiconset/32.png"
    ))
    .map_err(|err| format!("读取状态栏图标失败: {err}"))?
    .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height)
        .map_err(|err| format!("解析状态栏图标失败: {err}"))
}

pub fn app_is_active() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    NSApplication::sharedApplication(mtm).isActive()
}

pub fn activate_app() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    // `-[NSApplication activate]` was added in macOS 14. Calling it on
    // macOS 12/13 raises an Objective-C exception, which aborts across Rust's
    // event-loop callback. Keep the older API while those systems are supported.
    #[allow(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}
