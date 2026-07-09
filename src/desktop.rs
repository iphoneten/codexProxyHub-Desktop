use crate::{
    config::{default_config_path, ApiKeyConfig, AppConfig, ProviderConfig},
    proxy,
};
use eframe::egui;
use parking_lot::Mutex;
use std::{path::PathBuf, sync::Arc};
use tokio::{runtime::Runtime, sync::oneshot};

pub struct HubApp {
    config_path: String,
    config: Option<AppConfig>,
    selected_provider: Option<usize>,
    view: AppView,
    message: String,
    runtime: Option<Runtime>,
    server: Arc<Mutex<ServerHandle>>,
    new_key: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AppView {
    Overview,
    Providers,
    Auth,
    Routing,
}

#[derive(Default)]
struct ServerHandle {
    running: bool,
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    last_error: Option<String>,
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
            new_key: String::new(),
        };
        app.load_config();
        app
    }

    fn load_config(&mut self) {
        match AppConfig::load(PathBuf::from(self.config_path.trim())) {
            Ok(config) => {
                self.config = Some(config);
                self.selected_provider = Some(0);
                self.message = "配置已加载".to_string();
            }
            Err(err) => {
                self.config = None;
                self.message = err.to_string();
            }
        }
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
        let server_state = Arc::clone(&self.server);
        runtime.spawn(async move {
            let result = proxy::run_server(config, rx).await;
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
        } else {
            self.message = "代理未运行".to_string();
        }
    }
}

impl eframe::App for HubApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
            let Some(config) = self.config.as_mut() else {
                empty_state(
                    ui,
                    "未加载配置",
                    "请确认 config.yaml 路径，然后点击重新加载。",
                );
                return;
            };

            egui::Frame::none()
                .inner_margin(egui::Margin::symmetric(18.0, 16.0))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
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
                                AppView::Providers => {
                                    provider_section(
                                        ui,
                                        config,
                                        &mut self.selected_provider,
                                        &mut self.message,
                                    )
                                }
                                AppView::Auth => auth_section(ui, config, &mut self.new_key),
                                AppView::Routing => routing_section(ui, config),
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
                        egui::RichText::new("recodexProxyHub")
                            .size(24.0)
                            .strong()
                            .color(stat_color()),
                    );
                    ui.label(
                        egui::RichText::new("Rust desktop proxy console")
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
                });
            });
        });
}

fn status_bar(ui: &mut egui::Ui, app: &HubApp) {
    egui::Frame::none()
        .fill(surface())
        .stroke(egui::Stroke::new(1.0, border()))
        .inner_margin(egui::Margin::symmetric(18.0, 7.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("状态")
                        .strong()
                        .color(egui::Color32::from_rgb(70, 75, 84)),
                );
                ui.separator();
                ui.label(egui::RichText::new(&app.message).color(muteds()));
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

fn auth_section(ui: &mut egui::Ui, config: &mut AppConfig, new_key: &mut String) {
    section(ui, "鉴权", |ui| {
        ui.horizontal(|ui| {
            switch(ui, &mut config.auth.enabled);
            ui.label("启用 Bearer API Key 鉴权");
        });
        ui.horizontal(|ui| {
            ui.label("新 Key");
            ui.text_edit_singleline(new_key);
            if ui.button("添加").clicked() && !new_key.trim().is_empty() {
                config.auth.api_keys.push(ApiKeyConfig {
                    key: new_key.trim().to_string(),
                    name: format!("key-{}", config.auth.api_keys.len() + 1),
                    enabled: true,
                    created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                });
                new_key.clear();
            }
        });

        egui::Grid::new("keys").striped(true).show(ui, |ui| {
            ui.label("启用");
            ui.label("名称");
            ui.label("Key");
            ui.label("操作");
            ui.end_row();
            let mut remove = None;
            for (idx, key) in config.auth.api_keys.iter_mut().enumerate() {
                switch(ui, &mut key.enabled);
                ui.text_edit_singleline(&mut key.name);
                ui.monospace(&key.key);
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

fn routing_section(ui: &mut egui::Ui, config: &mut AppConfig) {
    section(ui, "模型降级", |ui| {
        if config.routing.model_fallbacks.is_empty() {
            ui.label("未配置 fallback");
        }
        let keys: Vec<String> = config.routing.model_fallbacks.keys().cloned().collect();
        for key in keys {
            let mut delete = false;
            ui.horizontal(|ui| {
                ui.label(&key);
                if let Some(values) = config.routing.model_fallbacks.get_mut(&key) {
                    let mut text = values.join(", ");
                    if ui.text_edit_singleline(&mut text).changed() {
                        *values = text
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(ToOwned::to_owned)
                            .collect();
                    }
                }
                if ui.button("删除").clicked() {
                    delete = true;
                }
            });
            if delete {
                config.routing.model_fallbacks.remove(&key);
            }
        }
        if ui.button("新增 fallback").clicked() {
            config
                .routing
                .model_fallbacks
                .insert("model-name".to_string(), vec!["fallback-model".to_string()]);
        }
    });
}

fn provider_section(
    ui: &mut egui::Ui,
    config: &mut AppConfig,
    selected: &mut Option<usize>,
    message: &mut String,
) {
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
            provider_detail_panel(ui, provider, message);
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

fn provider_detail_panel(ui: &mut egui::Ui, provider: &mut ProviderConfig, message: &mut String) {
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

                    form_label(ui, "Timeout");
                    ui.add(egui::DragValue::new(&mut provider.timeout).range(1..=600));
                    form_label(ui, "Retries");
                    ui.add(egui::DragValue::new(&mut provider.max_retries).range(0..=20));
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
                            *message = format!("已同步渠道 [{}] 的 {} 个模型", provider.name, count);
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
            let mut mapping_text = provider
                .model_mapping
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("\n");
            if ui
                .add(egui::TextEdit::multiline(&mut mapping_text).desired_rows(5))
                .changed()
            {
                provider.model_mapping.clear();
                for line in mapping_text.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let k = k.trim();
                        let v = v.trim();
                        if !k.is_empty() && !v.is_empty() {
                            provider.model_mapping.insert(k.to_string(), v.to_string());
                        }
                    }
                }
            }

            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("额外 Headers，Name=Value，每行一个")
                    .size(12.0)
                    .color(muteds()),
            );
            let mut headers_text = provider
                .extra_headers
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("\n");
            if ui
                .add(egui::TextEdit::multiline(&mut headers_text).desired_rows(4))
                .changed()
            {
                provider.extra_headers.clear();
                for line in headers_text.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let k = k.trim();
                        let v = v.trim();
                        if !k.is_empty() && !v.is_empty() {
                            provider.extra_headers.insert(k.to_string(), v.to_string());
                        }
                    }
                }
            }
        });
    });
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
        .timeout(std::time::Duration::from_secs(provider.timeout.max(1)))
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
            if let Some(id) = item
                .as_str()
                .or_else(|| item.get("id").or_else(|| item.get("name")).and_then(|value| value.as_str()))
            {
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
        timeout: 120,
        max_retries: 0,
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
