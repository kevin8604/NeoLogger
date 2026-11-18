mod app;
mod config;
mod demo;
mod log_view;
mod stats;
mod theme;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 820.0])
            .with_min_inner_size([940.0, 560.0])
            .with_title("NeoLogger — 日志分析工作台"),
        ..Default::default()
    };
    eframe::run_native(
        "NeoLogger",
        options,
        Box::new(|cc| Ok(Box::new(app::NeoLogApp::new(cc)))),
    )
}
