use dbtool::app::App;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let mut native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 500.0])
            .with_title("dbTool"),
        ..Default::default()
    };

    // WSLg's Wayland path (winit 0.29) kills the app during interactive window
    // resize; the same resize is stable over X11/Xwayland. Prefer X11 whenever
    // it's available — opt back into Wayland with DBTOOL_BACKEND=wayland.
    let force_wayland = std::env::var("DBTOOL_BACKEND").is_ok_and(|v| v == "wayland");
    if !force_wayland && std::env::var("DISPLAY").is_ok() {
        use winit::platform::x11::EventLoopBuilderExtX11;
        native_options.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11();
        }));
    }

    eframe::run_native(
        "dbTool",
        native_options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
