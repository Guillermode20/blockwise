mod app;
mod filter;
mod format;
mod model;
mod scanner;
mod settings;
mod treemap;
mod view_model;
#[cfg(test)]
mod verify_test;

use app::Blockwise;
use gpui::{
    AppContext, Application, Bounds, WindowBounds, WindowOptions, size, px,
};

fn main() {
    let initial_path = std::env::args().nth(1).or_else(|| std::env::var("BLOCKWISE_ROOT").ok());

    Application::new().run(move |cx: &mut gpui::App| {
        let settings = settings::Settings::load();
        let (w, h) = settings.window.unwrap_or((1200., 800.));
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        let path = initial_path.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // Matches the .desktop file so shells show the right icon.
                app_id: Some("io.github.Guillermode20.Blockwise".into()),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("blockwise".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                let font = app::pick_font(&cx.text_system().all_font_names());
                let settings = settings.clone();
                let view = cx.new(|cx| Blockwise::new(font, settings, cx));
                let v = view.clone();
                window.on_window_should_close(cx, move |window, cx| {
                    v.update(cx, |app, _| app.save_settings(window));
                    true
                });
                if let Some(p) = path.clone() {
                    let _ = view.update(cx, |v, cx| v.start_scan(p, cx));
                }
                view
            },
        )
        .unwrap();
        cx.activate(true);
    });
}