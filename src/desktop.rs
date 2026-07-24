use crate::{
    config::{default_config_path, AppConfig},
    proxy::{self, ConfigHandle},
};
use eframe::egui;
use parking_lot::{Mutex, RwLock};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::oneshot};

mod about;
mod analytics;
mod assets;
mod auth;
mod auth_accounts;
mod common;
mod logs;
mod overview;
mod providers;
mod settings;

use analytics::OverviewAnalyticsState;
use assets::AnimatedGif;
use logs::{LogTotals, LogViewState};

#[cfg(target_os = "macos")]
use crate::macos_tray::{activate_app, app_is_active, MacosTray, TrayAction};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MessageKind {
    Info,
    Success,
    Error,
}

impl Default for MessageKind {
    fn default() -> Self {
        Self::Info
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct AppMessage {
    pub(crate) text: String,
    pub(crate) kind: MessageKind,
}

impl Default for AppMessage {
    fn default() -> Self {
        Self {
            text: String::new(),
            kind: MessageKind::Info,
        }
    }
}

impl AppMessage {
    pub(crate) fn new(text: impl Into<String>, kind: MessageKind) -> Self {
        Self {
            text: text.into(),
            kind,
        }
    }
}

pub struct HubApp {
    config_path: String,
    config: Option<AppConfig>,
    selected_provider: Option<usize>,
    view: AppView,
    message: AppMessage,
    runtime: Option<Runtime>,
    server: Arc<Mutex<ServerHandle>>,
    new_key_name: String,
    provider_mapping_drafts: Vec<TextDraft>,
    provider_header_drafts: Vec<TextDraft>,
    routing_drafts: Vec<RoutingDraft>,
    routing_drafts_source: String,
    loading_gif: Option<AnimatedGif>,
    log_view: LogViewState,
    overview_analytics: OverviewAnalyticsState,
    auth_accounts_state: auth_accounts::AuthAccountsState,
    config_handle: Option<ConfigHandle>,
    circuit_status: Option<proxy::ProviderCircuitStatusHandle>,
    #[cfg(target_os = "macos")]
    tray: Option<MacosTray>,
    #[cfg(target_os = "macos")]
    quitting: bool,
    #[cfg(target_os = "macos")]
    window_hidden: bool,
    #[cfg(target_os = "macos")]
    app_was_active: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppView {
    Overview,
    Providers,
    AuthAccounts,
    Auth,
    Logs,
    Settings,
    About,
}

#[derive(Default)]
pub(crate) struct ServerHandle {
    pub(crate) running: bool,
    pub(crate) endpoint: String,
    pub(crate) shutdown: Option<oneshot::Sender<()>>,
    pub(crate) last_error: Option<String>,
    pub(crate) started_at: Option<Instant>,
}

#[derive(Default, Clone)]
pub(crate) struct TextDraft {
    pub(crate) text: String,
    pub(crate) source: String,
}

#[derive(Default, Clone)]
pub(crate) struct RoutingDraft {
    pub(crate) key: String,
    pub(crate) value: String,
}

impl HubApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        common::configure_style(&cc.egui_ctx);
        let runtime = Runtime::new().ok();
        let path = default_config_path();
        let mut app = Self {
            config_path: path.display().to_string(),
            config: None,
            selected_provider: None,
            view: AppView::Overview,
            message: AppMessage::default(),
            runtime,
            server: Arc::new(Mutex::new(ServerHandle::default())),
            new_key_name: String::new(),
            provider_mapping_drafts: Vec::new(),
            provider_header_drafts: Vec::new(),
            routing_drafts: Vec::new(),
            routing_drafts_source: String::new(),
            loading_gif: AnimatedGif::load(
                &cc.egui_ctx,
                "loading",
                include_bytes!("../assets/loading.gif"),
            ),
            log_view: LogViewState::default(),
            overview_analytics: OverviewAnalyticsState::default(),
            auth_accounts_state: auth_accounts::AuthAccountsState::default(),
            config_handle: None,
            circuit_status: None,
            #[cfg(target_os = "macos")]
            tray: None,
            #[cfg(target_os = "macos")]
            quitting: false,
            #[cfg(target_os = "macos")]
            window_hidden: false,
            #[cfg(target_os = "macos")]
            app_was_active: false,
        };
        app.load_config();
        #[cfg(target_os = "macos")]
        match MacosTray::new(&cc.egui_ctx) {
            Ok(tray) => app.tray = Some(tray),
            Err(err) => app.message = AppMessage::new(err, MessageKind::Error),
        }
        #[cfg(target_os = "macos")]
        {
            app.app_was_active = app_is_active();
        }
        app
    }

    fn load_config(&mut self) {
        match AppConfig::load(PathBuf::from(self.config_path.trim())) {
            Ok(config) => {
                self.replace_config(config);
                self.message = AppMessage::new("配置已加载", MessageKind::Success);
            }
            Err(err) => {
                self.config = None;
                self.provider_mapping_drafts.clear();
                self.provider_header_drafts.clear();
                self.routing_drafts.clear();
                self.routing_drafts_source.clear();
                self.log_view = LogViewState::default();
                self.overview_analytics = OverviewAnalyticsState::default();
                self.message = AppMessage::new(err.to_string(), MessageKind::Error);
            }
        }
    }

    fn replace_config(&mut self, config: AppConfig) {
        self.config = Some(config);
        self.selected_provider = Some(0);
        self.provider_mapping_drafts.clear();
        self.provider_header_drafts.clear();
        self.routing_drafts.clear();
        self.routing_drafts_source.clear();
        self.log_view = LogViewState::default();
        self.overview_analytics = OverviewAnalyticsState::default();
    }

    fn save_config(&mut self) {
        let Some(config) = self.config.as_ref() else {
            self.message = AppMessage::new("没有可保存的配置", MessageKind::Error);
            return;
        };
        match config.save(PathBuf::from(self.config_path.trim())) {
            Ok(()) => self.message = AppMessage::new("配置已保存", MessageKind::Success),
            Err(err) => self.message = AppMessage::new(err.to_string(), MessageKind::Error),
        }
    }

    fn import_config(&mut self) {
        let Some(source_path) = common::config_file_dialog(&self.config_path).pick_file() else {
            return;
        };
        let target_path = PathBuf::from(self.config_path.trim());

        match crate::config_import::import_file(&source_path, self.config.as_ref()) {
            Ok(imported) => match imported.config.save(&target_path) {
                Ok(()) => match AppConfig::load(&target_path) {
                    Ok(config) => {
                        self.replace_config(config);
                        let detail = if imported.replaced_config {
                            "配置已导入".to_string()
                        } else {
                            format!("已导入或更新 {} 个 Auth 账号", imported.imported_accounts)
                        };
                        self.message = AppMessage::new(
                            format!("{detail}: {}", source_path.display()),
                            MessageKind::Success,
                        );
                    }
                    Err(err) => self.message = AppMessage::new(err.to_string(), MessageKind::Error),
                },
                Err(err) => {
                    self.message =
                        AppMessage::new(format!("导入配置写入失败: {err}"), MessageKind::Error)
                }
            },
            Err(err) => {
                self.message = AppMessage::new(format!("导入配置无效: {err}"), MessageKind::Error)
            }
        }
    }

    fn export_config(&mut self) {
        let Some(config) = self.config.as_ref() else {
            self.message = AppMessage::new("没有可导出的配置", MessageKind::Error);
            return;
        };
        let Some(target_path) = common::config_file_dialog(&self.config_path)
            .set_file_name("config.yaml")
            .save_file()
        else {
            return;
        };

        match config.save(&target_path) {
            Ok(()) => {
                self.message = AppMessage::new(
                    format!("配置已导出: {}", target_path.display()),
                    MessageKind::Success,
                )
            }
            Err(err) => {
                self.message = AppMessage::new(format!("导出配置失败: {err}"), MessageKind::Error)
            }
        }
    }

    fn start_server(&mut self) {
        let Some(runtime) = self.runtime.as_ref() else {
            self.message = AppMessage::new("Tokio runtime 初始化失败", MessageKind::Error);
            return;
        };
        if self.server.lock().running {
            self.message = AppMessage::new("代理已经在运行", MessageKind::Error);
            return;
        }
        let Some(config) = self.config.clone() else {
            self.message = AppMessage::new("请先加载配置", MessageKind::Error);
            return;
        };
        if let Err(err) = proxy::validate_config(&config) {
            self.message = AppMessage::new(err.to_string(), MessageKind::Error);
            return;
        }

        let endpoint = format!(
            "http://{}:{}/v1",
            common::display_host(&config.server.host),
            config.server.port
        );
        let web_endpoints = config.web.enabled.then(|| {
            let host = common::display_host(&config.server.host);
            (
                format!("http://{}:{}/user", host, config.server.port),
                format!("http://{}:{}/admin", host, config.server.port),
            )
        });
        let (tx, rx) = oneshot::channel();
        {
            let mut server = self.server.lock();
            server.running = true;
            server.endpoint = endpoint.clone();
            server.shutdown = Some(tx);
            server.last_error = None;
            server.started_at = Some(Instant::now());
        }
        let handle: ConfigHandle = Arc::new(RwLock::new(Arc::new(config)));
        self.config_handle = Some(Arc::clone(&handle));
        let circuit_status = Arc::new(RwLock::new(std::collections::HashMap::new()));
        self.circuit_status = Some(Arc::clone(&circuit_status));
        let server_state = Arc::clone(&self.server);
        runtime.spawn(async move {
            let result = proxy::run_server(handle, rx, circuit_status).await;
            let mut server = server_state.lock();
            server.running = false;
            server.shutdown = None;
            server.started_at = None;
            if let Err(err) = result {
                server.last_error = Some(err.to_string());
            }
        });
        let started_message = match web_endpoints {
            Some((user, admin)) => {
                format!("代理已启动: {endpoint}，用户 Web: {user}，管理 Web: {admin}")
            }
            None => format!("代理已启动: {endpoint}"),
        };
        self.message = AppMessage::new(started_message, MessageKind::Success);
    }

    fn stop_server(&mut self) {
        let mut server = self.server.lock();
        if let Some(tx) = server.shutdown.take() {
            let _ = tx.send(());
            server.running = false;
            server.started_at = None;
            self.message = AppMessage::new("正在停止代理", MessageKind::Info);
            drop(server);
            self.config_handle = None;
            self.circuit_status = None;
        } else {
            self.message = AppMessage::new("代理未运行", MessageKind::Error);
        }
    }

    fn sync_config_to_runtime(&self) {
        if let (Some(handle), Some(cfg)) = (self.config_handle.as_ref(), self.config.as_ref()) {
            *handle.write() = Arc::new(cfg.clone());
        }
    }

    #[cfg(target_os = "macos")]
    fn handle_tray(&mut self, ctx: &egui::Context) {
        let app_active = app_is_active();
        if self.window_hidden && app_active && !self.app_was_active {
            self.show_main_window(ctx);
        }
        self.app_was_active = app_active;

        let running = self.server.lock().running;
        if let Some(tray) = self.tray.as_mut() {
            tray.set_running(running);
        }

        while let Some(action) = self.tray.as_ref().and_then(MacosTray::next_action) {
            match action {
                TrayAction::ShowWindow => self.show_main_window(ctx),
                TrayAction::ToggleServer => {
                    if self.server.lock().running {
                        self.stop_server();
                    } else {
                        self.start_server();
                    }
                }
                TrayAction::Quit => {
                    self.quitting = true;
                    if self.server.lock().running {
                        self.stop_server();
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn show_main_window(&mut self, ctx: &egui::Context) {
        self.window_hidden = false;
        activate_app();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }
}

impl eframe::App for HubApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "macos")]
        self.handle_tray(ctx);

        #[cfg(target_os = "macos")]
        if !self.quitting && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.window_hidden = true;
            self.message = AppMessage::new("窗口已隐藏，代理继续在状态栏运行", MessageKind::Info);
        }

        #[cfg(target_os = "windows")]
        if ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            self.message = AppMessage::new("窗口已最小化，代理继续运行", MessageKind::Info);
        }

        if let Some(config) = self.config.as_mut() {
            auth_accounts::tick_background_grok_check(config, &mut self.auth_accounts_state);
        }
        self.sync_config_to_runtime();

        egui::TopBottomPanel::top("top")
            .exact_height(74.0)
            .show(ctx, |ui| {
                top_bar(ui, self);
            });

        egui::TopBottomPanel::bottom("status")
            .exact_height(34.0)
            .show(ctx, |ui| {
                status_bar(ui, self);
            });

        egui::SidePanel::left("navigation")
            .resizable(false)
            .exact_width(180.0)
            .show(ctx, |ui| {
                side_navigation(ui, self);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Frame::none()
                .inner_margin(egui::Margin::symmetric(18.0, 16.0))
                .show(ui, |ui| {
                    if self.view == AppView::About {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                about::about_section(ui, &self.server.lock());
                            });
                        return;
                    }

                    let Some(config) = self.config.as_mut() else {
                        common::empty_state(
                            ui,
                            "未加载配置",
                            "请确认 config.yaml 路径，然后点击重新加载。",
                        );
                        return;
                    };

                    match self.view {
                        AppView::Overview => {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    analytics::overview_analytics_section(
                                        ui,
                                        config,
                                        &mut self.overview_analytics,
                                    );
                                    ui.add_space(12.0);
                                    overview::provider_summary_section(
                                        ui,
                                        config,
                                        &mut self.selected_provider,
                                        &mut self.view,
                                        &self.overview_analytics,
                                        self.circuit_status.as_ref(),
                                    );
                                });
                        }
                        AppView::Providers => {
                            providers::provider_section(
                                ui,
                                config,
                                &mut self.selected_provider,
                                &mut self.message,
                                &mut self.provider_mapping_drafts,
                                &mut self.provider_header_drafts,
                                self.circuit_status.as_ref(),
                            );
                        }
                        AppView::AuthAccounts => {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    auth_accounts::auth_accounts_section(
                                        ui,
                                        config,
                                        &mut self.message,
                                        &mut self.auth_accounts_state,
                                    );
                                });
                        }
                        AppView::Auth => {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    auth::auth_section(ui, config, &mut self.new_key_name);
                                });
                        }
                        AppView::Logs => {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    logs::logs_section(
                                        ui,
                                        config,
                                        &mut self.log_view,
                                        &mut self.message,
                                        self.loading_gif.as_ref(),
                                    );
                                });
                        }
                        AppView::Settings => {
                            egui::ScrollArea::vertical()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.set_max_width(640.0);
                                    settings::server_section(ui, config);
                                    ui.add_space(14.0);
                                    settings::auth_models_section(ui, config);
                                    ui.add_space(14.0);
                                    settings::routing_section(
                                        ui,
                                        config,
                                        &mut self.routing_drafts,
                                        &mut self.routing_drafts_source,
                                    );
                                });
                        }
                        AppView::About => {}
                    }
                });
        });

        let repaint_ms = if self.log_view.rows.iter().any(|row| row.status == "running")
            || self.auth_accounts_state.has_refreshing()
        {
            100
        } else {
            500
        };
        ctx.request_repaint_after(Duration::from_millis(repaint_ms));
    }
}

fn top_bar(ui: &mut egui::Ui, app: &mut HubApp) {
    egui::Frame::none()
        .fill(common::surface())
        .stroke(egui::Stroke::new(1.0, common::border()))
        .inner_margin(egui::Margin::symmetric(22.0, 12.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("RouteHub")
                            .size(24.0)
                            .strong()
                            .color(common::stat_color()),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "API 路由与代理网关控制台 · v{}",
                            crate::app_version()
                        ))
                        .size(12.0)
                        .color(common::muteds()),
                    );
                });
                ui.add_space(20.0);
                server_badge(ui, &app.server.lock());

                if let Some(ref config) = app.config {
                    let enabled_providers = config.providers.iter().filter(|p| p.enabled).count();
                    let model_count = auth::available_model_names(config).len();
                    let (auth_status, auth_color) = if config.auth.enabled {
                        ("开启", common::good())
                    } else {
                        ("关闭", common::muteds())
                    };

                    ui.add_space(16.0);
                    common::metric_badge(
                        ui,
                        "启用渠道",
                        &enabled_providers.to_string(),
                        common::accent(),
                    );
                    ui.add_space(8.0);
                    common::metric_badge(
                        ui,
                        "模型数量",
                        &model_count.to_string(),
                        egui::Color32::from_rgb(110, 106, 220),
                    );
                    ui.add_space(8.0);
                    common::metric_badge(ui, "安全鉴权", auth_status, auth_color);
                }

                let running = app.server.lock().running;

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button("⚙ 配置管理", |ui| {
                        if ui.button("💾 保存配置").clicked() {
                            app.save_config();
                            ui.close_menu();
                        }
                        if ui.button("🔄 重新加载").clicked() {
                            app.load_config();
                            ui.close_menu();
                        }
                        if ui.button("📤 导出配置").clicked() {
                            app.export_config();
                            ui.close_menu();
                        }
                        if ui.button("📥 导入配置").clicked() {
                            app.import_config();
                            ui.close_menu();
                        }
                    });
                    ui.add_space(8.0);
                    if running {
                        if common::danger_button(ui, "停止代理").clicked() {
                            app.stop_server();
                        }
                    } else if common::primary_button(ui, "启动代理").clicked() {
                        app.start_server();
                    }
                });
            });
        });
}

fn status_bar(ui: &mut egui::Ui, app: &HubApp) {
    egui::Frame::none()
        .fill(common::surface())
        .stroke(egui::Stroke::new(1.0, common::border()))
        .inner_margin(egui::Margin::symmetric(18.0, 7.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("操作动态")
                        .strong()
                        .color(egui::Color32::from_rgb(70, 75, 84)),
                );
                ui.separator();
                let text = if app.message.text.trim().is_empty() {
                    "暂无操作"
                } else {
                    app.message.text.as_str()
                };
                let color = match app.message.kind {
                    MessageKind::Info => common::muteds(),
                    MessageKind::Success => common::good(),
                    MessageKind::Error => egui::Color32::from_rgb(239, 68, 68),
                };
                ui.label(egui::RichText::new(text).color(color));
                if let Some(err) = app.server.lock().last_error.clone() {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(176, 54, 64), err);
                }
            });
        });
}

fn side_navigation(ui: &mut egui::Ui, app: &mut HubApp) {
    let mut next_view = app.view;
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(248, 250, 252))
        .stroke(egui::Stroke::NONE)
        .inner_margin(egui::Margin::symmetric(14.0, 16.0))
        .show(ui, |ui| {
            ui.set_min_height(ui.available_height());
            nav_item(ui, &mut next_view, AppView::Overview, "概览");
            nav_item(ui, &mut next_view, AppView::Providers, "渠道");
            nav_item(ui, &mut next_view, AppView::AuthAccounts, "Auth 账号");
            nav_item(ui, &mut next_view, AppView::Auth, "鉴权");
            nav_item(ui, &mut next_view, AppView::Logs, "日志");
            nav_item(ui, &mut next_view, AppView::Settings, "设置");
            nav_item(ui, &mut next_view, AppView::About, "关于");
        });
    app.view = next_view;
}

fn nav_item(ui: &mut egui::Ui, view: &mut AppView, target: AppView, title: &str) {
    let selected = *view == target;
    let fill = if selected {
        egui::Color32::from_rgb(229, 238, 255)
    } else {
        egui::Color32::from_rgb(248, 250, 252)
    };
    let response = egui::Frame::none()
        .fill(fill)
        .rounding(6.0)
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(title).strong().color(if selected {
                common::accent()
            } else {
                egui::Color32::from_rgb(70, 78, 92)
            }));
        })
        .response;
    if ui
        .interact(response.rect, response.id, egui::Sense::click())
        .clicked()
    {
        *view = target;
    }
    ui.add_space(4.0);
}

fn server_badge(ui: &mut egui::Ui, server: &ServerHandle) {
    let (text, fg, fill) = if server.running {
        (
            format!("运行中  {}", server.endpoint),
            common::good(),
            egui::Color32::from_rgb(220, 252, 231),
        )
    } else {
        (
            "未运行".to_string(),
            common::muteds(),
            egui::Color32::from_rgb(241, 245, 249),
        )
    };
    egui::Frame::none()
        .fill(fill)
        .rounding(999.0)
        .inner_margin(egui::Margin::symmetric(12.0, 6.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).strong().color(fg));
        });
}
