use eframe::egui;
use egui::Color32;

pub fn configure(ctx: &egui::Context) {
    // Pin the theme instead of following the OS: the stage is always dark, and
    // a mixed light/dark panel made the panel background look like a stray
    // vertical rectangle beside the cards.
    ctx.set_theme(egui::Theme::Dark);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = Color32::from_rgb(20, 22, 27);
    style.visuals.window_fill = Color32::from_rgb(24, 27, 33);
    style.visuals.extreme_bg_color = Color32::from_rgb(12, 14, 18);
    style.visuals.faint_bg_color = Color32::from_rgb(28, 32, 39);
    style.visuals.selection.bg_fill = Color32::from_rgb(52, 108, 170);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    ctx.set_style(style);
}
