use crate::config::AppConfig;
use eframe::egui;
use std::path::PathBuf;
use super::assets::AnimatedGif;

// --- 颜色常量 ---
pub fn accent() -> egui::Color32 {
    egui::Color32::from_rgb(37, 99, 235)
}

pub fn good() -> egui::Color32 {
    egui::Color32::from_rgb(16, 185, 129)
}

pub fn muteds() -> egui::Color32 {
    egui::Color32::from_rgb(100, 116, 139)
}

pub fn text_color() -> egui::Color32 {
    egui::Color32::from_rgb(51, 65, 85)
}

pub fn heading_color() -> egui::Color32 {
    egui::Color32::from_rgb(30, 41, 59)
}

pub fn stat_color() -> egui::Color32 {
    egui::Color32::from_rgb(15, 23, 42)
}

pub fn surface() -> egui::Color32 {
    egui::Color32::WHITE
}

pub fn border() -> egui::Color32 {
    egui::Color32::from_rgb(226, 232, 240)
}

// --- 按钮组件 ---
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
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

pub fn soft_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(text_color()))
            .fill(egui::Color32::from_rgb(244, 247, 251))
            .rounding(6.0)
            .min_size(egui::vec2(78.0, 32.0)),
    )
}

pub fn danger_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
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

// --- 常用 Widget & 装饰器 ---
pub fn switch(ui: &mut egui::Ui, value: &mut bool) -> egui::Response {
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

pub fn badge(ui: &mut egui::Ui, text: &str, fill: egui::Color32, color: egui::Color32) {
    egui::Frame::none()
        .fill(fill)
        .rounding(999.0)
        .inner_margin(egui::Margin::symmetric(8.0, 3.0))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(12.0).color(color));
        });
}

pub fn metric_badge(ui: &mut egui::Ui, label: &str, value: &str, color: egui::Color32) {
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(241, 245, 249))
        .rounding(6.0)
        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
        .show(ui, |ui| {
            ui.set_min_width(110.0);
            ui.horizontal(|ui| {
                metric_icon(ui, color);
                ui.label(
                    egui::RichText::new(format!("{}:", label))
                        .size(12.0)
                        .color(muteds()),
                );
                ui.label(
                    egui::RichText::new(value)
                        .size(12.0)
                        .strong()
                        .color(stat_color()),
                );
            });
        });
}

pub fn metric_tile(ui: &mut egui::Ui, label: &str, value: &str, detail: &str, color: egui::Color32) {
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

pub fn metric_icon(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

pub fn loading_icon(ui: &mut egui::Ui, loading_gif: Option<&AnimatedGif>, size: egui::Vec2) {
    if let Some(gif) = loading_gif {
        ui.add(egui::Image::new(gif.texture()).fit_to_exact_size(size));
    } else {
        metric_icon(ui, good());
    }
}

pub fn field_icon(ui: &mut egui::Ui, kind: &str) {
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

pub fn empty_state(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.centered_and_justified(|ui| {
        ui.vertical_centered(|ui| {
            ui.heading(title);
            ui.label(egui::RichText::new(detail).color(muteds()));
        });
    });
}

pub fn copy_icon_button(ui: &mut egui::Ui, value: &str) -> egui::Response {
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

pub fn table_header(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(12.0).color(muteds()));
}

// --- 表单与区块容器 ---
pub fn form_group(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(egui::Color32::from_rgb(248, 250, 252))
        .stroke(egui::Stroke::new(1.0, border()))
        .rounding(6.0)
        .inner_margin(egui::Margin::symmetric(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
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

pub fn form_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(12.0).color(muteds()));
}

pub fn section(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        egui::vec2(width, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            egui::Frame::none()
                .fill(surface())
                .stroke(egui::Stroke::new(1.0, border()))
                .rounding(8.0)
                .inner_margin(egui::Margin::symmetric(16.0, 14.0))
                .show(ui, |ui| {
                    ui.set_min_width(width - 32.0);
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
        },
    );
}

// --- 配置加载及初始化 ---
pub fn configure_style(ctx: &egui::Context) {
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

// --- 通用工具方法 ---
pub fn display_host(host: &str) -> &str {
    if host == "0.0.0.0" {
        "127.0.0.1"
    } else {
        host
    }
}

pub fn config_file_dialog(config_path: &str) -> rfd::FileDialog {
    let path = PathBuf::from(config_path.trim());
    let mut dialog = rfd::FileDialog::new().add_filter("YAML 配置", &["yaml", "yml"]);
    if let Some(parent) = path.parent().filter(|parent| parent.exists()) {
        dialog = dialog.set_directory(parent);
    }
    dialog
}

pub fn base_url(config: &AppConfig) -> String {
    format!(
        "http://{}:{}/v1",
        display_host(&config.server.host),
        config.server.port
    )
}

pub fn format_compact_tokens(n: i64) -> String {
    let sign = if n < 0 { "-" } else { "" };
    let abs = n.unsigned_abs();
    if abs < 1_000 {
        return format!("{sign}{abs}");
    }
    if abs < 1_000_000 {
        let hundredths = abs / 10;
        return format!("{sign}{}.{:02}K", hundredths / 100, hundredths % 100);
    }
    let hundredths = abs / 10_000;
    format!("{sign}{}.{:02}M", hundredths / 100, hundredths % 100)
}
