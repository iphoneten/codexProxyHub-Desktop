use super::common::{accent, badge, muteds, section, soft_button, stat_color, text_color};
use super::ServerHandle;
use eframe::egui;

pub fn about_section(ui: &mut egui::Ui, server: &ServerHandle) {
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
            if let Some(started_at) = server.started_at {
                about_info_row(ui, "运行时长", &format_duration(started_at.elapsed()));
            }
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
        });
    });
}

fn format_duration(duration: std::time::Duration) -> String {
    let total = duration.as_secs();
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;
    if days > 0 {
        format!("{days}天 {hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
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
