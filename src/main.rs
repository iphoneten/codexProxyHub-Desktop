mod config;
mod desktop;
mod proxy;

fn main() -> eframe::Result<()> {
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
