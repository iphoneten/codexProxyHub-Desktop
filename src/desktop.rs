use crate::{
    config::{default_config_path, ApiKeyConfig, AppConfig, ProviderConfig},
    proxy::{self, ConfigHandle},
};
use eframe::egui;
use parking_lot::{Mutex, RwLock};
use std::{path::PathBuf, sync::Arc, time::Instant};
use tokio::{runtime::Runtime, sync::oneshot};

#[cfg(target_os = "macos")]
use crate::macos_tray::{activate_app, app_is_active, MacosTray, TrayAction};

pub struct HubApp {
    config_path: String,
    config: Option<AppConfig>,
    selected_provider: Option<usize>,
    view: AppView,
    message: String,
    runtime: Option<Runtime>,
    server: Arc<Mutex<ServerHandle>>,
    new_key_name: String,
    provider_mapping_drafts: Vec<TextDraft>,
    provider_header_drafts: Vec<TextDraft>,
    log_view: LogViewState,
    // 运行中的代理服务器共享的配置句柄。UI 修改会推送到这里，服务器每次请求读取最新
    config_handle: Option<ConfigHandle>,
    keepalive_status: Option<proxy::KeepaliveStatusHandle>,
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
enum AppView {
    Overview,
    Providers,
    Auth,
    Routing,
    Logs,
    About,
}

#[derive(Default)]
struct ServerHandle {
    running: bool,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    last_error: Option<String>,
}

#[derive(Default, Clone)]
struct TextDraft {
    text: String,
    source: String,
}

struct LogViewState {
    rows: Vec<LogRow>,
    loaded_path: String,
    page: usize,
    total: usize,
    last_refresh: Option<Instant>,
    totals: LogTotals,
    live: bool,
}

impl Default for LogViewState {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            loaded_path: String::new(),
            page: 0,
            total: 0,
            last_refresh: None,
            totals: LogTotals::default(),
            live: true,
        }
    }
}

#[derive(Default, Clone, Copy)]
struct LogTotals {
    input_tokens: i64,
    output_tokens: i64,
}

impl HubApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx);
        let runtime = Runtime::new().ok();
        let path = default_config_path();
        let mut app = Self {
            config_path: path.display().to_string(),
            config: None,
            selected_provider: None,
            view: AppView::Overview,
            message: String::new(),
            runtime,
            server: Arc::new(Mutex::new(ServerHandle::default())),
            new_key_name: String::new(),
            provider_mapping_drafts: Vec::new(),
            provider_header_drafts: Vec::new(),
            log_view: LogViewState::default(),
            config_handle: None,
            keepalive_status: None,
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
            Err(err) => app.message = err,
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
                self.message = "配置已加载".to_string();
            }
            Err(err) => {
                self.config = None;
                self.provider_mapping_drafts.clear();
                self.provider_header_drafts.clear();
                self.log_view = LogViewState::default();
                self.message = err.to_string();
            }
        }
    }

    fn replace_config(&mut self, config: AppConfig) {
        self.config = Some(config);
        self.selected_provider = Some(0);
        self.provider_mapping_drafts.clear();
        self.provider_header_drafts.clear();
        self.log_view = LogViewState::default();
    }

    fn save_config(&mut self) {
        let Some(config) = self.config.as_ref() else {
            self.message = "没有可保存的配置".to_string();
            return;
        };
        match config.save(PathBuf::from(self.config_path.trim())) {
            Ok(()) => self.message = "配置已保存".to_string(),
            Err(err) => self.message = err.to_string(),
        }
    }

    fn import_config(&mut self) {
        let Some(source_path) = config_file_dialog(&self.config_path).pick_file() else {
            return;
        };
        let target_path = PathBuf::from(self.config_path.trim());

        match AppConfig::load(&source_path) {
            Ok(imported) => match imported.save(&target_path) {
                Ok(()) => match AppConfig::load(&target_path) {
                    Ok(config) => {
                        self.replace_config(config);
                        self.message = format!("配置已导入: {}", source_path.display());
                    }
                    Err(err) => self.message = err.to_string(),
                },
                Err(err) => self.message = format!("导入配置写入失败: {err}"),
            },
            Err(err) => self.message = format!("导入配置无效: {err}"),
        }
    }

    fn export_config(&mut self) {
        let Some(config) = self.config.as_ref() else {
            self.message = "没有可导出的配置".to_string();
            return;
        };
        let Some(target_path) = config_file_dialog(&self.config_path)
            .set_file_name("config.yaml")
            .save_file()
        else {
            return;
        };

        match config.save(&target_path) {
            Ok(()) => self.message = format!("配置已导出: {}", target_path.display()),
            Err(err) => self.message = format!("导出配置失败: {err}"),
        }
    }

    fn start_server(&mut self) {
        let Some(runtime) = self.runtime.as_ref() else {
            self.message = "Tokio runtime 初始化失败".to_string();
            return;
        };
        if self.server.lock().running {
            self.message = "代理已经在运行".to_string();
            return;
        }
        let Some(config) = self.config.clone() else {
            self.message = "请先加载配置".to_string();
            return;
        };
        if let Err(err) = proxy::validate_config(&config) {
            self.message = err.to_string();
            return;
        }

        let endpoint = format!(
            "http://{}:{}/v1",
            display_host(&config.server.host),
            config.server.port
        );
        let (tx, rx) = oneshot::channel();
        {
            let mut server = self.server.lock();
            server.running = true;
            server.endpoint = endpoint.clone();
            server.shutdown = Some(tx);
            server.last_error = None;
        }
        // 建立配置句柄：UI 与运行中的 server 共享同一 Arc<RwLock<Arc<AppConfig>>>
        let handle: ConfigHandle = Arc::new(RwLock::new(Arc::new(config)));
        self.config_handle = Some(Arc::clone(&handle));
        let keepalive_status = Arc::new(RwLock::new(std::collections::HashMap::new()));
        self.keepalive_status = Some(Arc::clone(&keepalive_status));
        let server_state = Arc::clone(&self.server);
        runtime.spawn(async move {
            let result = proxy::run_server(handle, rx, keepalive_status).await;
            let mut server = server_state.lock();
            server.running = false;
            server.shutdown = None;
            if let Err(err) = result {
                server.last_error = Some(err.to_string());
            }
        });
        self.message = format!("代理已启动: {}", endpoint);
    }

    fn stop_server(&mut self) {
        let mut server = self.server.lock();
        if let Some(tx) = server.shutdown.take() {
            let _ = tx.send(());
            server.running = false;
            self.message = "正在停止代理".to_string();
            drop(server);
            self.config_handle = None;
            self.keepalive_status = None;
        } else {
            self.message = "代理未运行".to_string();
        }
    }

    // 若代理在跑，把 UI 侧最新配置推送到运行时。每帧调用一次即可
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
            self.message = "窗口已隐藏，代理继续在状态栏运行".to_string();
        }

        // 把 UI 修改推送到运行中的代理，确保 provider.enabled 等改动即时生效
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
            .exact_width(248.0)
            .show(ctx, |ui| {
                side_navigation(ui, self);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Frame::none()
                .inner_margin(egui::Margin::symmetric(18.0, 16.0))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if self.view == AppView::About {
                                about_section(ui, &self.config_path, &self.server.lock());
                                return;
                            }

                            let Some(config) = self.config.as_mut() else {
                                empty_state(
                                    ui,
                                    "未加载配置",
                                    "请确认 config.yaml 路径，然后点击重新加载。",
                                );
                                return;
                            };

                            overview_header(ui, config, &self.server.lock());
                            ui.add_space(14.0);
                            match self.view {
                                AppView::Overview => {
                                    server_section(ui, config);
                                    ui.add_space(12.0);
                                    provider_summary_section(
                                        ui,
                                        config,
                                        &mut self.selected_provider,
                                        &mut self.view,
                                    );
                                }
                                AppView::Providers => provider_section(
                                    ui,
                                    config,
                                    &mut self.selected_provider,
                                    &mut self.message,
                                    &mut self.provider_mapping_drafts,
                                    &mut self.provider_header_drafts,
                                    self.keepalive_status.as_ref(),
                                ),
                                AppView::Auth => auth_section(ui, config, &mut self.new_key_name),
                                AppView::Routing => routing_section(ui, config),
                                AppView::Logs => {
                                    logs_section(ui, config, &mut self.log_view, &mut self.message)
                                }
                                AppView::About => {}
                            }
                        });
                });
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

fn top_bar(ui: &mut egui::Ui, app: &mut HubApp) {
    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .inner_margin(egui::Margin::symmetric(22.0, 12.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("RouteHub")
                            .size(24.0)
                            .strong()
                            .color(stat_color()),
                    );
                    ui.label(
                        egui::RichText::new(format!(
                            "Rust desktop proxy console · v{}",
                            crate::app_version()
                        ))
                        .size(12.0)
                        .color(muteds()),
                    );
                });
                ui.add_space(20.0);
                server_badge(ui, &app.server.lock());
                let running = app.server.lock().running;

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        if danger_button(ui, "停止代理").clicked() {
                            app.stop_server();
                        }
                    } else if primary_button(ui, "启动代理").clicked() {
                        app.start_server();
                    }
                    if soft_button(ui, "保存配置").clicked() {
                        app.save_config();
                    }
                    if soft_button(ui, "重新加载").clicked() {
                        app.load_config();
                    }
                    if soft_button(ui, "导出配置").clicked() {
                        app.export_config();
                    }
                    if soft_button(ui, "导入配置").clicked() {
                        app.import_config();
                    }
                });
            });
        });
}

fn config_file_dialog(config_path: &str) -> rfd::FileDialog {
    let path = PathBuf::from(config_path.trim());
    let mut dialog = rfd::FileDialog::new().add_filter("YAML 配置", &["yaml", "yml"]);
    if let Some(parent) = path.parent().filter(|parent| parent.exists()) {
        dialog = dialog.set_directory(parent);
    }
    dialog
}

fn status_bar(ui: &mut egui::Ui, app: &HubApp) {
    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .inner_margin(egui::Margin::symmetric(18.0, 7.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("操作动态")
                        .strong()
                        .color(egui::Color32::from_rgb(70, 75, 84)),
                );
                ui.separator();
                let message = if app.message.trim().is_empty() {
                    "暂无操作"
                } else {
                    app.message.as_str()
                };
                ui.label(egui::RichText::new(message).color(muteds()));
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
        .stroke(egui::Stroke::new(1.0, border()))
        .inner_margin(egui::Margin::symmetric(14.0, 16.0))
        .show(ui, |ui| {
            ui.set_min_height(ui.available_height());
            nav_item(ui, &mut next_view, AppView::Overview, "概览");
            nav_item(ui, &mut next_view, AppView::Providers, "渠道");
            nav_item(ui, &mut next_view, AppView::Auth, "鉴权");
            nav_item(ui, &mut next_view, AppView::Routing, "路由");
            nav_item(ui, &mut next_view, AppView::Logs, "日志");
            nav_item(ui, &mut next_view, AppView::About, "关于");
            ui.add_space(18.0);
            ui.separator();
            ui.add_space(10.0);

            ui.label(
                egui::RichText::new("OpenAI-compatible local proxy")
                    .small()
                    .color(muteds()),
            );
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
                accent()
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

fn overview_header(ui: &mut egui::Ui, config: &AppConfig, server: &ServerHandle) {
    let enabled = config.providers.iter().filter(|p| p.enabled).count();
    let model_count: usize = config
        .providers
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.models.len())
        .sum();
    ui.horizontal_wrapped(|ui| {
        metric_tile(
            ui,
            "代理状态",
            if server.running {
                "运行中"
            } else {
                "未运行"
            },
            server.endpoint.as_str(),
            if server.running { good() } else { muteds() },
        );
        metric_tile(
            ui,
            "启用渠道",
            &enabled.to_string(),
            "参与模型路由",
            accent(),
        );
        metric_tile(
            ui,
            "模型数量",
            &model_count.to_string(),
            "来自启用渠道",
            egui::Color32::from_rgb(110, 106, 220),
        );
        metric_tile(
            ui,
            "鉴权",
            if config.auth.enabled {
                "开启"
            } else {
                "关闭"
            },
            &format!("{} keys", config.auth.api_keys.len()),
            if config.auth.enabled {
                good()
            } else {
                muteds()
            },
        );
    });
}

fn metric_tile(ui: &mut egui::Ui, label: &str, value: &str, detail: &str, color: egui::Color32) {
    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(14.0, 12.0))
        .show(ui, |ui| {
            ui.set_min_width(180.0);
            ui.horizontal(|ui| {
                metric_icon(ui, color);
                ui.label(egui::RichText::new(label).size(12.0).color(muteds()));
            });
            ui.label(
                egui::RichText::new(value)
                    .size(28.0)
                    .strong()
                    .color(stat_color()),
            );
            ui.label(egui::RichText::new(detail).size(12.0).color(muteds()));
        });
}

fn metric_icon(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

fn provider_summary_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    selected_provider: &mut Option<usize>,
    view: &mut AppView,
) {
    section(ui, "渠道概览", |ui| {
        egui::Grid::new("provider_summary")
            .striped(true)
            .min_col_width(92.0)
            .show(ui, |ui| {
                table_header(ui, "状态");
                table_header(ui, "名称");
                table_header(ui, "类型");
                table_header(ui, "优先级");
                table_header(ui, "模型");
                ui.end_row();
                for (idx, provider) in config.providers.iter_mut().enumerate() {
                    switch(ui, &mut provider.enabled);
                    if ui.link(&provider.name).clicked() {
                        *selected_provider = Some(idx);
                        *view = AppView::Providers;
                    }
                    ui.label(&provider.provider_type);
                    ui.horizontal(|ui| {
                        badge(
                            ui,
                            &format!("P{}", provider.priority),
                            egui::Color32::from_rgb(239, 246, 255),
                            accent(),
                        );
                        badge(
                            ui,
                            &format!("W{}", provider.weight),
                            egui::Color32::from_rgb(240, 253, 244),
                            good(),
                        );
                    });
                    badge(
                        ui,
                        &provider.models.len().to_string(),
                        egui::Color32::from_rgb(248, 250, 252),
                        text_color(),
                    );
                    ui.end_row();
                }
            });
    });
}

fn empty_state(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            ui.heading(title);
            ui.label(egui::RichText::new(detail).color(muteds()));
        });
    });
}

fn server_section(ui: &mut egui::Ui, config: &mut AppConfig) {
    section(ui, "服务", |ui| {
        ui.horizontal(|ui| {
            field_icon(ui, "network");
            ui.label("Host");
            ui.text_edit_singleline(&mut config.server.host);
            field_icon(ui, "plug");
            ui.label("Port");
            let mut port = config.server.port as i64;
            if ui
                .add(egui::DragValue::new(&mut port).range(1..=65535))
                .changed()
            {
                config.server.port = port as u16;
            }
        });
        let url = base_url(config);
        ui.horizontal(|ui| {
            ui.label("Base URL");
            let mut readonly_url = url.clone();
            ui.add_enabled(
                false,
                egui::TextEdit::singleline(&mut readonly_url).desired_width(320.0),
            );
            copy_icon_button(ui, &url).on_hover_text("复制 Base URL");
        });
    });
}

fn base_url(config: &AppConfig) -> String {
    format!(
        "http://{}:{}/v1",
        display_host(&config.server.host),
        config.server.port
    )
}

fn copy_icon_button(ui: &mut egui::Ui, value: &str) -> egui::Response {
    let size = egui::vec2(28.0, 28.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let fill = if response.hovered() {
        egui::Color32::from_rgb(239, 246, 255)
    } else {
        surface()
    };
    ui.painter().rect_filled(rect, 6.0, fill);
    ui.painter()
        .rect_stroke(rect, 6.0, egui::Stroke::new(1.0, border()));

    let back = egui::Rect::from_min_size(rect.min + egui::vec2(8.0, 7.0), egui::vec2(9.0, 11.0));
    let front = egui::Rect::from_min_size(rect.min + egui::vec2(11.0, 10.0), egui::vec2(9.0, 11.0));
    ui.painter()
        .rect_stroke(back, 2.0, egui::Stroke::new(1.4, muteds()));
    ui.painter().rect_filled(front, 2.0, fill);
    ui.painter()
        .rect_stroke(front, 2.0, egui::Stroke::new(1.4, accent()));

    if response.clicked() {
        ui.output_mut(|output| {
            output.copied_text = value.to_string();
        });
    }
    response
}

fn badge(ui: &mut egui::Ui, text: &str, fill: egui::Color32, color: egui::Color32) {
    egui::Frame::none()
        .fill(fill)
        .rounding(999.0)
        .inner_margin(egui::Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(12.0).color(color));
        });
}

fn table_header(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(12.0).color(muteds()));
}

fn field_icon(ui: &mut egui::Ui, kind: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
    let painter = ui.painter();
    let color = muteds();
    match kind {
        "network" => {
            let c1 = rect.left_center() + egui::vec2(4.0, 0.0);
            let c2 = rect.center_top() + egui::vec2(0.0, 5.0);
            let c3 = rect.right_center() - egui::vec2(4.0, 0.0);
            painter.line_segment([c1, c2], egui::Stroke::new(1.2, color));
            painter.line_segment([c2, c3], egui::Stroke::new(1.2, color));
            painter.circle_filled(c1, 2.2, color);
            painter.circle_filled(c2, 2.2, color);
            painter.circle_filled(c3, 2.2, color);
        }
        "plug" => {
            let body = egui::Rect::from_center_size(rect.center(), egui::vec2(8.0, 9.0));
            painter.rect_stroke(body, 2.0, egui::Stroke::new(1.3, color));
            painter.line_segment(
                [
                    body.left_top() + egui::vec2(2.0, -4.0),
                    body.left_top() + egui::vec2(2.0, 0.0),
                ],
                egui::Stroke::new(1.3, color),
            );
            painter.line_segment(
                [
                    body.right_top() + egui::vec2(-2.0, -4.0),
                    body.right_top() + egui::vec2(-2.0, 0.0),
                ],
                egui::Stroke::new(1.3, color),
            );
            painter.line_segment(
                [
                    body.center_bottom(),
                    body.center_bottom() + egui::vec2(0.0, 4.0),
                ],
                egui::Stroke::new(1.3, color),
            );
        }
        _ => {}
    }
}

fn switch(ui: &mut egui::Ui, value: &mut bool) -> egui::Response {
    let desired_size = egui::vec2(40.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }

    let t = ui.ctx().animate_bool(response.id, *value);
    let bg = if *value {
        good()
    } else {
        egui::Color32::from_rgb(203, 213, 225)
    };
    let stroke = if *value {
        egui::Stroke::new(1.0, good())
    } else {
        egui::Stroke::new(1.0, egui::Color32::from_rgb(148, 163, 184))
    };
    ui.painter().rect_filled(rect, 11.0, bg);
    ui.painter().rect_stroke(rect, 11.0, stroke);

    let knob_radius = 8.0;
    let left = rect.left() + 11.0;
    let right = rect.right() - 11.0;
    let knob_x = left + (right - left) * t;
    ui.painter().circle_filled(
        egui::pos2(knob_x, rect.center().y),
        knob_radius,
        egui::Color32::WHITE,
    );
    response
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(text)
                .strong()
                .color(egui::Color32::WHITE),
        )
        .fill(accent())
        .rounding(6.0)
        .min_size(egui::vec2(86.0, 32.0)),
    )
}

fn soft_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(text_color()))
            .fill(egui::Color32::from_rgb(244, 247, 251))
            .rounding(6.0)
            .min_size(egui::vec2(78.0, 32.0)),
    )
}

fn danger_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(text)
                .strong()
                .color(egui::Color32::WHITE),
        )
        .fill(egui::Color32::from_rgb(239, 68, 68))
        .rounding(6.0)
        .min_size(egui::vec2(86.0, 32.0)),
    )
}

fn accent() -> egui::Color32 {
    egui::Color32::from_rgb(37, 99, 235)
}

fn good() -> egui::Color32 {
    egui::Color32::from_rgb(16, 185, 129)
}

fn muteds() -> egui::Color32 {
    egui::Color32::from_rgb(100, 116, 139)
}

fn text_color() -> egui::Color32 {
    egui::Color32::from_rgb(51, 65, 85)
}

fn heading_color() -> egui::Color32 {
    egui::Color32::from_rgb(30, 41, 59)
}

fn stat_color() -> egui::Color32 {
    egui::Color32::from_rgb(15, 23, 42)
}

fn surface() -> egui::Color32 {
    egui::Color32::from_rgb(255, 255, 255)
}

fn border() -> egui::Color32 {
    egui::Color32::from_rgb(226, 232, 240)
}

/// 生成随机 API Key，格式与既有配置保持一致：`sk-proxy-` + 32 位小写字母数字。
fn generate_api_key() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let suffix: String = (0..32)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    format!("sk-proxy-{}", suffix)
}

fn auth_section(ui: &mut egui::Ui, config: &mut AppConfig, new_key_name: &mut String) {
    section(ui, "鉴权", |ui| {
        ui.horizontal(|ui| {
            switch(ui, &mut config.auth.enabled);
            ui.label("启用 API 秘钥鉴权");
        });
        ui.horizontal(|ui| {
            ui.label("新秘钥名称");
            ui.add(egui::TextEdit::singleline(new_key_name).desired_width(220.0));
            if ui.button("添加").clicked() && !new_key_name.trim().is_empty() {
                let name = new_key_name.trim().to_string();
                let final_name = if name.is_empty() {
                    format!("key-{}", config.auth.api_keys.len() + 1)
                } else {
                    name
                };
                config.auth.api_keys.push(ApiKeyConfig {
                    key: generate_api_key(),
                    name: final_name,
                    enabled: true,
                    created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    max_concurrency: Some(5),
                });
                new_key_name.clear();
            }
        });

        egui::Grid::new("keys")
            .striped(true)
            .spacing(egui::vec2(14.0, 8.0))
            .show(ui, |ui| {
                ui.label("启用");
                ui.label("名称");
                ui.label("并发");
                ui.label("API 秘钥");
                ui.label("创建时间");
                ui.label("操作");
                ui.end_row();
                let mut remove = None;
                for (idx, key) in config.auth.api_keys.iter_mut().enumerate() {
                    switch(ui, &mut key.enabled);
                    ui.add_sized([120.0, 24.0], egui::TextEdit::singleline(&mut key.name));
                    let max_concurrency = key.max_concurrency.get_or_insert(5);
                    ui.add(
                        egui::DragValue::new(max_concurrency)
                            .range(1..=500)
                            .speed(1),
                    );
                    ui.monospace(mask_api_key(&key.key));
                    ui.label(if key.created_at.trim().is_empty() {
                        "-"
                    } else {
                        key.created_at.as_str()
                    });
                    ui.horizontal(|ui| {
                        if ui.button("复制").clicked() {
                            ui.output_mut(|output| {
                                output.copied_text = key.key.clone();
                            });
                        }
                        if ui.button("删除").clicked() {
                            remove = Some(idx);
                        }
                    });
                    ui.end_row();
                }
                if let Some(idx) = remove {
                    config.auth.api_keys.remove(idx);
                }
            });
    });
}

fn mask_api_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 12 {
        return "*".repeat(len.max(6));
    }
    let prefix: String = key.chars().take(8).collect();
    let suffix: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}****{suffix}")
}

fn routing_section(ui: &mut egui::Ui, config: &mut AppConfig) {
    section(ui, "模型映射", |ui| {
        if config.routing.model_fallbacks.is_empty() {
            ui.label("未配置映射");
        }
        // 取出 entries 到 Vec，使两侧均可编辑（HashMap 遍历时无法修改 key）。
        let mut entries: Vec<(String, Vec<String>)> = config
            .routing
            .model_fallbacks
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut to_remove: Option<usize> = None;
        for (idx, (src, dsts)) in entries.iter_mut().enumerate() {
            let mut dst_text = dsts.join(", ");
            ui.horizontal(|ui| {
                ui.text_edit_singleline(src);
                ui.label("→");
                if ui.text_edit_singleline(&mut dst_text).changed() {
                    *dsts = dst_text
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned)
                        .collect();
                }
                if ui.button("删除").clicked() {
                    to_remove = Some(idx);
                }
            });
        }
        if let Some(idx) = to_remove {
            entries.remove(idx);
        }
        if ui.button("新增映射").clicked() {
            entries.push(("model-name".to_string(), vec!["target-model".to_string()]));
        }
        // 写回 HashMap，空 key 丢弃。
        config.routing.model_fallbacks.clear();
        for (k, v) in entries {
            let key = k.trim().to_string();
            if !key.is_empty() {
                config.routing.model_fallbacks.insert(key, v);
            }
        }
    });
}

fn logs_section(
    ui: &mut egui::Ui,
    config: &AppConfig,
    log_view: &mut LogViewState,
    message: &mut String,
) {
    const LOG_PAGE_SIZE: usize = 20;
    let path = config.usage_log_sqlite_path();
    let source_key = format!("sqlite:{}", path.display());
    if log_view.loaded_path != source_key {
        log_view.page = 0;
        refresh_logs(config, log_view, Some(message));
    } else if log_view.live
        && log_view
            .last_refresh
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(1))
            .unwrap_or(true)
    {
        // 定时静默刷新：保留当前分页与状态栏消息
        refresh_logs(config, log_view, None);
    }

    section(ui, "请求日志", |ui| {
        ui.horizontal(|ui| {
            if soft_button(ui, "刷新").clicked() {
                refresh_logs(config, log_view, Some(message));
            }
            switch(ui, &mut log_view.live).on_hover_text("自动刷新请求日志");
            ui.label(
                egui::RichText::new(if log_view.live { "实时" } else { "已暂停" })
                    .color(if log_view.live { good() } else { muteds() }),
            );
            if soft_button(ui, "清空").clicked() {
                match clear_logs(config) {
                    Ok(()) => {
                        log_view.page = 0;
                        refresh_logs(config, log_view, None);
                        *message = "已清空日志".to_string();
                    }
                    Err(err) => *message = err,
                }
            }
        });
        // Token 使用统计（当前日志表/文件全部记录合计）
        let totals = log_view.totals;
        ui.horizontal_wrapped(|ui| {
            metric_tile(
                ui,
                "输入",
                &format_compact_tokens(totals.input_tokens),
                "累计 prompt tokens",
                accent(),
            );
            metric_tile(
                ui,
                "输出",
                &format_compact_tokens(totals.output_tokens),
                "累计 completion tokens",
                egui::Color32::from_rgb(110, 106, 220),
            );
            metric_tile(
                ui,
                "总 Token",
                &format_compact_tokens(totals.input_tokens + totals.output_tokens),
                "",
                good(),
            );
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let total_pages = log_view.total.div_ceil(LOG_PAGE_SIZE).max(1);
            if ui
                .add_enabled(log_view.page > 0, egui::Button::new("上一页"))
                .clicked()
            {
                log_view.page = log_view.page.saturating_sub(1);
                refresh_logs(config, log_view, Some(message));
            }
            ui.label(format!(
                "第 {} / {} 页，共 {} 条",
                log_view.page + 1,
                total_pages,
                log_view.total
            ));
            if ui
                .add_enabled(log_view.page + 1 < total_pages, egui::Button::new("下一页"))
                .clicked()
            {
                log_view.page += 1;
                refresh_logs(config, log_view, Some(message));
            }
        });
        ui.add_space(8.0);
        if log_view.rows.is_empty() {
            ui.label(egui::RichText::new("暂无日志").color(muteds()));
            return;
        }

        egui::Frame::none()
            .fill(egui::Color32::from_rgb(248, 250, 252))
            .stroke(egui::Stroke::new(1.0, border()))
            .rounding(6.0)
            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("usage_logs")
                            .striped(true)
                            .min_col_width(80.0)
                            .show(ui, |ui| {
                                table_header(ui, "时间");
                                table_header(ui, "状态");
                                table_header(ui, "API Key");
                                table_header(ui, "接口");
                                table_header(ui, "渠道");
                                table_header(ui, "模型");
                                table_header(ui, "Token");
                                table_header(ui, "用时/首字(秒)");
                                table_header(ui, "错误");
                                ui.end_row();

                                for row in &log_view.rows {
                                    ui.monospace(&row.ts);
                                    ui.label(status_text(&row.status));
                                    ui.label(row.api_key_label());
                                    ui.label(&row.api);
                                    ui.label(&row.channel);
                                    model_cell(ui, row);
                                    ui.label(format!("{}/{}", row.input_tokens, row.output_tokens));
                                    ui.label(format!(
                                        "{}/{}",
                                        row.display_latency(),
                                        row.first_token
                                    ));
                                    ui.label(egui::RichText::new(&row.error).color(muteds()));
                                    ui.end_row();
                                }
                            });
                    });
            });
    });
}

fn refresh_logs(config: &AppConfig, log_view: &mut LogViewState, message: Option<&mut String>) {
    const LOG_PAGE_SIZE: usize = 20;
    let path = config.usage_log_sqlite_path();
    log_view.loaded_path = format!("sqlite:{}", path.display());
    log_view.last_refresh = Some(Instant::now());
    let (final_rows, final_total, note) = match read_log_page(config, log_view.page, LOG_PAGE_SIZE)
    {
        Ok((rows, total)) => {
            let total_pages = total.div_ceil(LOG_PAGE_SIZE).max(1);
            if log_view.page >= total_pages {
                log_view.page = total_pages - 1;
                match read_log_page(config, log_view.page, LOG_PAGE_SIZE) {
                    Ok((rows2, total2)) => {
                        let count = rows2.len();
                        (rows2, total2, Ok(format!("已加载 {} 条日志", count)))
                    }
                    Err(err) => (Vec::new(), 0, Err(err)),
                }
            } else {
                let count = rows.len();
                (rows, total, Ok(format!("已加载 {} 条日志", count)))
            }
        }
        Err(err) => (Vec::new(), 0, Err(err)),
    };
    log_view.total = final_total;
    log_view.rows = final_rows;
    // 聚合 token 总量。读失败保留旧值，避免闪回 0
    if let Ok(totals) = read_log_totals(config) {
        log_view.totals = totals;
    }
    if let Some(msg) = message {
        match note {
            Ok(text) | Err(text) => *msg = text,
        }
    }
}

fn read_log_totals(config: &AppConfig) -> Result<LogTotals, String> {
    read_sqlite_log_totals(config.usage_log_sqlite_path())
}

fn read_sqlite_log_totals(path: PathBuf) -> Result<LogTotals, String> {
    if !path.exists() {
        return Ok(LogTotals::default());
    }
    let conn = proxy::open_usage_log_connection(path)
        .map_err(|err| format!("打开 SQLite 日志失败: {err}"))?;
    proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化 SQLite 日志表失败: {err}"))?;
    let (input_tokens, output_tokens) = conn
        .query_row(
            "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0) FROM usage_logs",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|err| format!("汇总 SQLite Token 失败: {err}"))?;
    Ok(LogTotals {
        input_tokens,
        output_tokens,
    })
}

// Token 紧凑显示：< 1K 原样；≥ 1K 用 K；≥ 1M 用 M，均保留 2 位小数
// 采用整数除法向下截断，避免浮点四舍五入导致边界值跨单位（如 999999 变 1000.00K）
fn format_compact_tokens(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "" };
    let abs = n.unsigned_abs();
    if abs < 1_000 {
        return format!("{sign}{abs}");
    }
    if abs < 1_000_000 {
        // 每 1K 拆成 100 份小数位；abs/10 直接得到小数位总数
        let hundredths = abs / 10;
        return format!("{sign}{}.{:02}K", hundredths / 100, hundredths % 100);
    }
    let hundredths = abs / 10_000;
    format!("{sign}{}.{:02}M", hundredths / 100, hundredths % 100)
}

fn clear_logs(config: &AppConfig) -> Result<(), String> {
    clear_sqlite_logs(config.usage_log_sqlite_path())
}

fn clear_sqlite_logs(path_buf: PathBuf) -> Result<(), String> {
    if !path_buf.exists() {
        return Ok(());
    }
    let conn = proxy::open_usage_log_connection(path_buf)
        .map_err(|err| format!("打开 SQLite 日志失败: {err}"))?;
    proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化 SQLite 日志表失败: {err}"))?;
    conn.execute("DELETE FROM usage_logs", [])
        .map_err(|err| format!("清空 SQLite 日志失败: {err}"))?;
    Ok(())
}

fn read_log_page(
    config: &AppConfig,
    page: usize,
    page_size: usize,
) -> Result<(Vec<LogRow>, usize), String> {
    read_sqlite_log_page(config.usage_log_sqlite_path(), page, page_size)
}

fn read_sqlite_log_page(
    path: PathBuf,
    page: usize,
    page_size: usize,
) -> Result<(Vec<LogRow>, usize), String> {
    if !path.exists() {
        return Ok((Vec::new(), 0));
    }
    let conn = proxy::open_usage_log_connection(path)
        .map_err(|err| format!("打开 SQLite 日志失败: {err}"))?;
    proxy::ensure_usage_log_schema(&conn)
        .map_err(|err| format!("初始化 SQLite 日志表失败: {err}"))?;
    let total = conn
        .query_row("SELECT COUNT(*) FROM usage_logs", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|err| format!("读取 SQLite 日志数量失败: {err}"))? as usize;
    let mut stmt = conn
        .prepare(
            r#"
            SELECT
                ts, status, api_key_name, api, channel, request_model, upstream_model,
                latency_ms, first_token_ms, input_tokens, output_tokens, error
            FROM usage_logs
            ORDER BY id DESC
            LIMIT ?1
            OFFSET ?2
            "#,
        )
        .map_err(|err| format!("读取 SQLite 日志失败: {err}"))?;
    let rows = stmt
        .query_map(
            [page_size as i64, page.saturating_mul(page_size) as i64],
            |row| {
                let latency_ms: i64 = row.get(7)?;
                let first_token_ms: Option<i64> = row.get(8)?;
                Ok(LogRow {
                    ts: row.get(0)?,
                    status: row.get(1)?,
                    api_key_name: row.get(2)?,
                    api: row.get(3)?,
                    channel: row.get(4)?,
                    model: row.get(5)?,
                    upstream_model: row.get(6)?,
                    first_token: first_token_ms
                        .map(|value| format!("{:.2}", value as f64 / 1000.0))
                        .unwrap_or_else(|| "-".to_string()),
                    latency: format!("{:.2}", latency_ms as f64 / 1000.0),
                    input_tokens: row.get(9)?,
                    output_tokens: row.get(10)?,
                    error: row.get(11)?,
                })
            },
        )
        .map_err(|err| format!("读取 SQLite 日志失败: {err}"))?;
    // 时间倒序：最新的一条排在最上面（SQL 已用 ORDER BY id DESC）
    let out = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("解析 SQLite 日志失败: {err}"))?;
    Ok((out, total))
}

#[derive(Clone)]
struct LogRow {
    ts: String,
    status: String,
    api_key_name: String,
    api: String,
    channel: String,
    model: String,
    upstream_model: String,
    first_token: String,
    latency: String,
    input_tokens: i64,
    output_tokens: i64,
    error: String,
}

impl LogRow {
    fn api_key_label(&self) -> &str {
        if self.api_key_name.trim().is_empty() {
            "-"
        } else {
            self.api_key_name.as_str()
        }
    }

    fn display_latency(&self) -> String {
        if self.status != "running" {
            return self.latency.clone();
        }
        chrono::NaiveDateTime::parse_from_str(&self.ts, "%Y-%m-%d %H:%M:%S")
            .ok()
            .map(|started| {
                let elapsed = chrono::Local::now().naive_local() - started;
                format!("{:.2}", elapsed.num_milliseconds().max(0) as f64 / 1000.0)
            })
            .unwrap_or_else(|| self.latency.clone())
    }
}

fn model_cell(ui: &mut egui::Ui, row: &LogRow) {
    ui.vertical(|ui| {
        ui.label(&row.model);
        // 上游模型为空时不渲染 ↳ 那一行，避免冗余
        if !row.upstream_model.trim().is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                forward_arrow(ui);
                ui.label(
                    egui::RichText::new(&row.upstream_model)
                        .size(11.0)
                        .color(muteds()),
                );
            });
        }
    });
}

fn forward_arrow(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    let stroke = egui::Stroke::new(1.2, muteds());
    let bend = egui::pos2(rect.left() + 2.0, rect.center().y);
    let tip = egui::pos2(rect.right() - 2.0, rect.center().y);
    ui.painter()
        .line_segment([egui::pos2(bend.x, rect.top() + 1.0), bend], stroke);
    ui.painter().line_segment([bend, tip], stroke);
    ui.painter()
        .line_segment([egui::pos2(tip.x - 3.0, tip.y - 3.0), tip], stroke);
    ui.painter()
        .line_segment([tip, egui::pos2(tip.x - 3.0, tip.y + 3.0)], stroke);
}

fn status_text(status: &str) -> egui::RichText {
    let (label, color) = if status == "running" {
        ("运行中", accent())
    } else if status == "ok" || status == "stream_started" {
        ("成功", good())
    } else if status == "-" || status == "raw" {
        (status, muteds())
    } else {
        ("失败", egui::Color32::from_rgb(176, 54, 64))
    };
    egui::RichText::new(label).strong().color(color)
}

fn provider_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    selected: &mut Option<usize>,
    message: &mut String,
    mapping_drafts: &mut Vec<TextDraft>,
    header_drafts: &mut Vec<TextDraft>,
    keepalive_status: Option<&proxy::KeepaliveStatusHandle>,
) {
    resize_drafts(mapping_drafts, config.providers.len());
    resize_drafts(header_drafts, config.providers.len());

    if config.providers.is_empty() {
        section(ui, "渠道", |ui| {
            ui.label(egui::RichText::new("暂无渠道").color(muteds()));
            if primary_button(ui, "新增渠道").clicked() {
                config.providers.push(default_provider());
                *selected = Some(0);
            }
        });
        return;
    }

    let mut idx = selected.unwrap_or(0).min(config.providers.len() - 1);
    let list_width = 280.0;
    let gap = 14.0;
    let detail_width = (ui.available_width() - list_width - gap).max(360.0);
    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(list_width, ui.available_height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                provider_list_panel(ui, config, &mut idx);
            },
        );
        ui.add_space(gap);
        ui.allocate_ui_with_layout(
            egui::vec2(detail_width, ui.available_height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                if idx >= config.providers.len() {
                    ui.label("渠道索引无效");
                    return;
                }
                *selected = Some(idx);
                let provider = &mut config.providers[idx];
                provider_detail_panel(
                    ui,
                    provider,
                    message,
                    &mut mapping_drafts[idx],
                    &mut header_drafts[idx],
                    keepalive_status,
                );
            },
        );
    });
}

fn provider_list_panel(ui: &mut egui::Ui, config: &mut AppConfig, selected: &mut usize) {
    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(12.0, 12.0))
        .show(ui, |ui| {
            ui.set_width(260.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("渠道列表")
                        .size(18.0)
                        .strong()
                        .color(heading_color()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if soft_button(ui, "新增").clicked() {
                        config.providers.push(default_provider());
                        *selected = config.providers.len() - 1;
                    }
                });
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (idx, provider) in config.providers.iter_mut().enumerate() {
                        let active = idx == *selected;
                        let fill = if active {
                            egui::Color32::from_rgb(239, 246, 255)
                        } else {
                            surface()
                        };
                        let stroke = if active {
                            egui::Stroke::new(1.0, egui::Color32::from_rgb(191, 219, 254))
                        } else {
                            egui::Stroke::new(1.0, border())
                        };
                        let response = egui::Frame::none()
                            .fill(fill)
                            .stroke(stroke)
                            .rounding(6.0)
                            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    switch(ui, &mut provider.enabled);
                                    ui.vertical(|ui| {
                                        ui.label(
                                            egui::RichText::new(&provider.name)
                                                .size(14.0)
                                                .strong()
                                                .color(if active {
                                                    accent()
                                                } else {
                                                    text_color()
                                                }),
                                        );
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{} · {} models",
                                                provider.provider_type,
                                                provider.models.len()
                                            ))
                                            .size(12.0)
                                            .color(muteds()),
                                        );
                                    });
                                });
                            })
                            .response;
                        if ui
                            .interact(response.rect, response.id, egui::Sense::click())
                            .clicked()
                        {
                            *selected = idx;
                        }
                        ui.add_space(6.0);
                    }
                });
        });
}

fn provider_detail_panel(
    ui: &mut egui::Ui,
    provider: &mut ProviderConfig,
    message: &mut String,
    mapping_draft: &mut TextDraft,
    header_draft: &mut TextDraft,
    keepalive_status: Option<&proxy::KeepaliveStatusHandle>,
) {
    section(ui, "渠道详情", |ui| {
        ui.horizontal_wrapped(|ui| {
            switch(ui, &mut provider.enabled);
            ui.label("启用");
            badge(
                ui,
                &provider.provider_type,
                egui::Color32::from_rgb(239, 246, 255),
                accent(),
            );
            badge(
                ui,
                &format!("{} models", provider.models.len()),
                egui::Color32::from_rgb(248, 250, 252),
                text_color(),
            );
        });
        ui.add_space(10.0);

        form_group(ui, "基础信息", |ui| {
            egui::Grid::new("provider_basic_form")
                .num_columns(2)
                .spacing(egui::vec2(14.0, 10.0))
                .show(ui, |ui| {
                    form_label(ui, "名称");
                    ui.text_edit_singleline(&mut provider.name);
                    ui.end_row();

                    form_label(ui, "类型");
                    egui::ComboBox::from_id_source("provider_type")
                        .selected_text(&provider.provider_type)
                        .show_ui(ui, |ui| {
                            for item in [
                                "openai",
                                "google_ai_studio",
                                "anthropic",
                                "codex_only",
                                "custom",
                            ] {
                                ui.selectable_value(
                                    &mut provider.provider_type,
                                    item.to_string(),
                                    item,
                                );
                            }
                        });
                    ui.end_row();

                    form_label(ui, "Base URL");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut provider.base_url);
                        copy_icon_button(ui, &provider.base_url)
                            .on_hover_text("复制 Provider Base URL");
                    });
                    ui.end_row();

                    form_label(ui, "API Key");
                    ui.add(egui::TextEdit::singleline(&mut provider.api_key).password(true));
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                form_label(ui, "运行状态");
                if let Some(status_handle) = keepalive_status {
                    let status = status_handle.read().get(&provider.name).cloned();
                    match status {
                        Some(status) => {
                            if let Some(success) = status.last_success {
                                ui.colored_label(good(), format!("最近成功 {}", success));
                            } else {
                                ui.label(egui::RichText::new("等待首次心跳").color(muteds()));
                            }
                            if let Some(error) = status.last_error {
                                ui.colored_label(
                                    egui::Color32::from_rgb(176, 54, 64),
                                    format!("最近失败: {}", error),
                                );
                            }
                        }
                        None => {
                            ui.label(egui::RichText::new("等待首次心跳").color(muteds()));
                        }
                    }
                } else {
                    ui.label(egui::RichText::new("代理未启动").color(muteds()));
                }
            });
        });

        ui.add_space(12.0);
        form_group(ui, "路由参数", |ui| {
            egui::Grid::new("provider_route_form")
                .num_columns(4)
                .spacing(egui::vec2(14.0, 10.0))
                .show(ui, |ui| {
                    form_label(ui, "Priority");
                    ui.add(egui::DragValue::new(&mut provider.priority).speed(1));
                    form_label(ui, "Weight");
                    ui.add(egui::DragValue::new(&mut provider.weight).range(1..=100));
                    ui.end_row();

                    form_label(ui, "Connect");
                    ui.add(egui::DragValue::new(&mut provider.connect_timeout).range(1..=120));
                    form_label(ui, "Request");
                    ui.add(egui::DragValue::new(&mut provider.request_timeout).range(1..=600));
                    ui.end_row();

                    form_label(ui, "Retries");
                    ui.add(egui::DragValue::new(&mut provider.max_retries).range(0..=50));
                    form_label(ui, "Stream Idle");
                    ui.add(egui::DragValue::new(&mut provider.stream_idle_timeout).range(0..=3600));
                    ui.end_row();

                    form_label(ui, "Stream Max");
                    ui.add(
                        egui::DragValue::new(&mut provider.stream_max_duration).range(0..=86400),
                    );
                    ui.label(egui::RichText::new("0 表示关闭").color(muteds()));
                    ui.label("");
                    ui.end_row();

                    form_label(ui, "Responses");
                    egui::ComboBox::from_id_source("responses_mode")
                        .selected_text(&provider.responses_mode)
                        .show_ui(ui, |ui| {
                            for item in ["auto", "native", "chat"] {
                                ui.selectable_value(
                                    &mut provider.responses_mode,
                                    item.to_string(),
                                    item,
                                );
                            }
                        });
                    form_label(ui, "Strip Thought");
                    ui.horizontal(|ui| {
                        switch(ui, &mut provider.strip_thought);
                        ui.label("过滤");
                    });
                    ui.end_row();

                    form_label(ui, "抓取 SSE");
                    ui.horizontal(|ui| {
                        switch(ui, &mut provider.debug_capture_sse);
                        ui.label("启用");
                    });
                    form_label(ui, "保留事件");
                    ui.add(
                        egui::DragValue::new(&mut provider.debug_sse_max_events).range(10..=500),
                    );
                    ui.end_row();

                    form_label(ui, "抓取目录");
                    ui.add(
                        egui::TextEdit::singleline(&mut provider.debug_sse_path)
                            .desired_width(220.0),
                    );
                    ui.label(egui::RichText::new("排查后建议关闭").color(muteds()));
                    ui.label("");
                    ui.end_row();
                });
        });

        ui.add_space(12.0);
        form_group(ui, "连接保活", |ui| {
            egui::Grid::new("provider_keepalive_form")
                .num_columns(4)
                .spacing(egui::vec2(14.0, 10.0))
                .show(ui, |ui| {
                    form_label(ui, "心跳");
                    ui.horizontal(|ui| {
                        switch(ui, &mut provider.persist_keepalive);
                        ui.label("启用");
                    });
                    form_label(ui, "间隔(秒)");
                    ui.add(
                        egui::DragValue::new(&mut provider.persist_keepalive_interval)
                            .range(5..=86400),
                    );
                    ui.end_row();

                    form_label(ui, "模型");
                    let model = provider
                        .persist_keepalive_model
                        .get_or_insert_with(String::new);
                    ui.add(egui::TextEdit::singleline(model).desired_width(220.0));
                    form_label(ui, "提示词");
                    ui.add(
                        egui::TextEdit::singleline(&mut provider.persist_keepalive_prompt)
                            .desired_width(220.0),
                    );
                    ui.end_row();
                });
        });

        ui.add_space(12.0);
        form_group(ui, "模型与转发", |ui| {
            ui.label(
                egui::RichText::new("系统提示词覆盖")
                    .size(12.0)
                    .color(muteds()),
            );
            ui.text_edit_multiline(
                provider
                    .system_prompt_override
                    .get_or_insert_with(String::new),
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("模型列表，每行一个")
                        .size(12.0)
                        .color(muteds()),
                );
                if soft_button(ui, "同步上游模型").clicked() {
                    match sync_upstream_models(provider) {
                        Ok(models) => {
                            let count = models.len();
                            provider.models = models;
                            *message =
                                format!("已同步渠道 [{}] 的 {} 个模型", provider.name, count);
                        }
                        Err(err) => {
                            *message = format!("同步渠道 [{}] 模型失败: {err}", provider.name);
                        }
                    }
                }
            });
            let mut models_text = provider.models.join("\n");
            if ui
                .add(egui::TextEdit::multiline(&mut models_text).desired_rows(9))
                .changed()
            {
                provider.models = models_text
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
            }

            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("模型映射，本地=上游，每行一个")
                    .size(12.0)
                    .color(muteds()),
            );
            sync_draft_from_source(mapping_draft, serialize_model_mapping(provider));
            if ui
                .add(egui::TextEdit::multiline(&mut mapping_draft.text).desired_rows(5))
                .changed()
            {
                provider.model_mapping.clear();
                for line in mapping_draft.text.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let k = k.trim();
                        let v = v.trim();
                        if !k.is_empty() && !v.is_empty() {
                            provider.model_mapping.insert(k.to_string(), v.to_string());
                        }
                    }
                }
                mapping_draft.source = serialize_model_mapping(provider);
            }

            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("额外 Headers，Name=Value，每行一个")
                    .size(12.0)
                    .color(muteds()),
            );
            sync_draft_from_source(header_draft, serialize_extra_headers(provider));
            if ui
                .add(egui::TextEdit::multiline(&mut header_draft.text).desired_rows(4))
                .changed()
            {
                provider.extra_headers.clear();
                for line in header_draft.text.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let k = k.trim();
                        let v = v.trim();
                        if !k.is_empty() && !v.is_empty() {
                            provider.extra_headers.insert(k.to_string(), v.to_string());
                        }
                    }
                }
                header_draft.source = serialize_extra_headers(provider);
            }
        });
    });
}

fn resize_drafts(drafts: &mut Vec<TextDraft>, len: usize) {
    drafts.resize_with(len, TextDraft::default);
}

fn sync_draft_from_source(draft: &mut TextDraft, source: String) {
    if draft.text.is_empty() && draft.source.is_empty() {
        draft.text = source.clone();
        draft.source = source;
        return;
    }
    if draft.source != source && draft.text == draft.source {
        draft.text = source.clone();
        draft.source = source;
    }
}

fn serialize_model_mapping(provider: &ProviderConfig) -> String {
    let mut entries: Vec<_> = provider.model_mapping.iter().collect();
    entries.sort_by_key(|(key, _)| *key);
    entries
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn serialize_extra_headers(provider: &ProviderConfig) -> String {
    let mut entries: Vec<_> = provider.extra_headers.iter().collect();
    entries.sort_by_key(|(key, _)| *key);
    entries
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn form_group(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(248, 250, 252))
        .stroke(egui::Stroke::new(1.0, border()))
        .rounding(6.0)
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(title)
                    .size(12.0)
                    .strong()
                    .color(muteds()),
            );
            ui.add_space(8.0);
            add_contents(ui);
        });
}

fn form_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(12.0).color(muteds()));
}

fn sync_upstream_models(provider: &ProviderConfig) -> Result<Vec<String>, String> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            provider.connect_timeout.max(1),
        ))
        .timeout(std::time::Duration::from_secs(
            provider.request_timeout.max(1),
        ))
        .build()
        .map_err(|err| err.to_string())?;

    let mut request = client.get(url);
    if provider.provider_type == "anthropic" {
        request = request
            .header("x-api-key", &provider.api_key)
            .header("anthropic-version", "2023-06-01");
    } else {
        request = request.bearer_auth(&provider.api_key);
    }
    for (name, value) in &provider.extra_headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "authorization" | "content-type" | "accept" | "host" | "content-length" | "x-api-key"
        ) {
            continue;
        }
        request = request.header(name, value);
    }

    let response = request.send().map_err(|err| err.to_string())?;
    let status = response.status();
    let payload: serde_json::Value = response.json().map_err(|err| err.to_string())?;
    if !status.is_success() {
        return Err(format!("上游返回 {status}: {payload}"));
    }

    let mut models = Vec::new();
    if let Some(data) = payload.get("data").and_then(|value| value.as_array()) {
        for item in data {
            if let Some(id) = item
                .get("id")
                .or_else(|| item.get("name"))
                .and_then(|value| value.as_str())
            {
                push_unique(&mut models, id);
            }
        }
    } else if let Some(data) = payload.get("models").and_then(|value| value.as_array()) {
        for item in data {
            if let Some(id) = item.as_str().or_else(|| {
                item.get("id")
                    .or_else(|| item.get("name"))
                    .and_then(|value| value.as_str())
            }) {
                push_unique(&mut models, id);
            }
        }
    }

    if models.is_empty() {
        Err("响应中没有可识别的模型列表".to_string())
    } else {
        models.sort();
        Ok(models)
    }
}

fn push_unique(models: &mut Vec<String>, id: &str) {
    let id = id.trim();
    if !id.is_empty() && !models.iter().any(|item| item == id) {
        models.push(id.to_string());
    }
}

fn about_section(ui: &mut egui::Ui, config_path: &str, server: &ServerHandle) {
    section(ui, "关于 RouteHub", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("RouteHub")
                    .size(24.0)
                    .strong()
                    .color(stat_color()),
            );
            badge(
                ui,
                &format!("v{}", crate::app_version()),
                egui::Color32::from_rgb(239, 246, 255),
                accent(),
            );
        });
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(
                "本地 OpenAI-compatible 代理桌面工具，用于管理多渠道转发、智能路由、故障转移和请求日志。",
            )
            .color(muteds()),
        );
    });

    ui.add_space(12.0);

    section(ui, "运行信息", |ui| {
        about_info_row(ui, "版本", crate::app_version());
        about_info_row(ui, "配置文件", config_path);
        about_info_row(
            ui,
            "代理状态",
            if server.running {
                "运行中"
            } else {
                "未运行"
            },
        );
        if server.running {
            about_info_row(ui, "本地接口", &server.endpoint);
        }
    });

    ui.add_space(12.0);

    section(ui, "项目", |ui| {
        let repository = "https://github.com/iphoneten/codexProxyHub-Desktop";
        about_info_row(ui, "仓库", repository);
        ui.horizontal(|ui| {
            if soft_button(ui, "复制仓库地址").clicked() {
                ui.output_mut(|output| output.copied_text = repository.to_string());
            }
            if soft_button(ui, "复制配置路径").clicked() {
                ui.output_mut(|output| output.copied_text = config_path.to_string());
            }
        });
    });
}

fn about_info_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.set_min_height(24.0);
        ui.add_sized(
            egui::vec2(92.0, 20.0),
            egui::Label::new(egui::RichText::new(label).size(12.0).color(muteds())),
        );
        ui.label(egui::RichText::new(value).color(text_color()));
    });
}

fn section(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .rounding(8.0)
        .inner_margin(egui::Margin::symmetric(16.0, 14.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .size(18.0)
                        .strong()
                        .color(heading_color()),
                );
                ui.add_space(8.0);
                add_contents(ui);
            });
        });
}

fn server_badge(ui: &mut egui::Ui, server: &ServerHandle) {
    let (text, fg, fill) = if server.running {
        (
            format!("运行中  {}", server.endpoint),
            good(),
            egui::Color32::from_rgb(220, 252, 231),
        )
    } else {
        (
            "未运行".to_string(),
            muteds(),
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

fn display_host(host: &str) -> &str {
    if host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        host
    }
}

fn default_provider() -> ProviderConfig {
    ProviderConfig {
        name: "new-provider".to_string(),
        enabled: false,
        provider_type: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        website: None,
        api_key: String::new(),
        models: vec!["gpt-5.5".to_string()],
        model_mapping: Default::default(),
        extra_headers: Default::default(),
        capabilities: Default::default(),
        health_check_mode: "models".to_string(),
        model_sync_filter: "all".to_string(),
        responses_mode: "auto".to_string(),
        client_mode: "normal".to_string(),
        connect_timeout: 10,
        request_timeout: 60,
        stream_idle_timeout: 0,
        stream_max_duration: 0,
        debug_capture_sse: false,
        debug_sse_path: "logs/raw_sse".to_string(),
        debug_sse_max_events: 80,
        max_retries: 3,
        weight: 1,
        priority: 1,
        description: None,
        system_prompt_override: None,
        strip_thought: false,
        persistent_session: false,
        persist_interval: 3.0,
        persist_max_wait: 0,
        persist_keepalive: false,
        persist_keepalive_interval: 30,
        persist_keepalive_model: None,
        persist_keepalive_prompt: "Hi".to_string(),
        extra: Default::default(),
    }
}

fn configure_style(ctx: &egui::Context) {
    configure_fonts(ctx);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.window_margin = egui::Margin::same(0.0);
    style.text_styles.insert(
        egui::TextStyle::Heading,
        egui::FontId::new(24.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        egui::FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        egui::FontId::new(12.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(14.0, egui::FontFamily::Monospace),
    );
    style.visuals.panel_fill = egui::Color32::from_rgb(248, 250, 252);
    style.visuals.window_fill = egui::Color32::from_rgb(248, 250, 252);
    style.visuals.extreme_bg_color = surface();
    style.visuals.widgets.inactive.bg_fill = surface();
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(248, 250, 252);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgb(239, 246, 255);
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, border());
    style.visuals.widgets.hovered.bg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(147, 197, 253));
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent());
    style.visuals.window_rounding = 6.0.into();
    style.visuals.widgets.active.rounding = 4.0.into();
    style.visuals.widgets.hovered.rounding = 4.0.into();
    style.visuals.widgets.inactive.rounding = 4.0.into();
    ctx.set_style(style);
}

fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
        "/System/Library/Fonts/Supplemental/Hiragino Sans GB.ttc",
    ];

    for path in candidates {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        fonts
            .font_data
            .insert("cjk".to_string(), egui::FontData::from_owned(data));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_string());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "cjk".to_string());
        ctx.set_fonts(fonts);
        return;
    }

    ctx.set_fonts(fonts);
}
