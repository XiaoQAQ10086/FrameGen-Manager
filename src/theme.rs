//! 浅色主题：调色板、尺寸、全局样式微调，以及卡片 / 徽章 / 按钮组件。
//!
//! 只使用 egui 内置能力，不引入任何新依赖，也不碰其他业务模块。

use egui::{Color32, CornerRadius, Frame, Margin, RichText, Shadow, Stroke, Ui};

// ---------------------------------------------------------------- 调色板（浅色）

pub const BG_APP: Color32 = Color32::from_rgb(0xF4, 0xF6, 0xF8);
pub const BG_CARD: Color32 = Color32::WHITE;
pub const BG_SUNKEN: Color32 = Color32::from_rgb(0xF1, 0xF3, 0xF6);
pub const BG_HOVER: Color32 = Color32::from_rgb(0xEA, 0xEE, 0xF4);
pub const BORDER: Color32 = Color32::from_rgb(0xDF, 0xE3, 0xE9);
pub const BORDER_STRONG: Color32 = Color32::from_rgb(0xC6, 0xCD, 0xD6);

pub const TEXT: Color32 = Color32::from_rgb(0x1F, 0x23, 0x28);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);

/// NVIDIA 绿。绿色填充上配深色文字才能保证对比度。
pub const ACCENT: Color32 = Color32::from_rgb(0x76, 0xB9, 0x00);
pub const ACCENT_BORDER: Color32 = Color32::from_rgb(0x63, 0x9E, 0x00);
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(0x14, 0x1C, 0x00);

pub const OK: Color32 = Color32::from_rgb(0x1B, 0x7F, 0x3B);
pub const WARN: Color32 = Color32::from_rgb(0xB2, 0x6A, 0x00);
pub const DANGER: Color32 = Color32::from_rgb(0xC6, 0x28, 0x28);
pub const NEUTRAL: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);

// ---------------------------------------------------------------- 尺寸

pub const R_CARD: u8 = 10;
pub const R_CTRL: u8 = 6;
pub const PAD_CARD: i8 = 12;

fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// 反作弊等级 -> 颜色
pub fn tier_color(tier: crate::anticheat::AcTier) -> Color32 {
    use crate::anticheat::AcTier;
    match tier {
        AcTier::None => OK,
        AcTier::UserMode => WARN,
        AcTier::Kernel => DANGER,
    }
}

// ---------------------------------------------------------------- 全局样式

/// 只做浅色，不做主题切换：把两个主题槽位都设成同一套浅色 Visuals。
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Light);

    ctx.all_styles_mut(|style| {
        let mut v = egui::Visuals::light();
        v.dark_mode = false;
        v.panel_fill = BG_APP;
        v.window_fill = BG_CARD;
        v.extreme_bg_color = BG_SUNKEN;
        v.faint_bg_color = Color32::from_rgb(0xEE, 0xF1, 0xF5);
        v.window_stroke = Stroke::new(1.0, BORDER);
        v.window_corner_radius = CornerRadius::same(R_CARD);
        v.menu_corner_radius = CornerRadius::same(R_CTRL);
        v.override_text_color = Some(TEXT);
        v.hyperlink_color = Color32::from_rgb(0x2E, 0x6B, 0x1E);
        v.selection.bg_fill = alpha(ACCENT, 60);
        v.selection.stroke = Stroke::new(1.0, ACCENT_BORDER);
        v.window_shadow = Shadow {
            offset: [0, 2],
            blur: 10,
            spread: 0,
            color: Color32::from_black_alpha(20),
        };

        {
            let w = &mut v.widgets;

            w.noninteractive.bg_fill = BG_CARD;
            w.noninteractive.weak_bg_fill = BG_CARD;
            w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
            w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
            w.noninteractive.corner_radius = CornerRadius::same(R_CTRL);

            w.inactive.bg_fill = Color32::from_rgb(0xED, 0xF0, 0xF4);
            w.inactive.weak_bg_fill = Color32::from_rgb(0xED, 0xF0, 0xF4);
            w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
            w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
            w.inactive.corner_radius = CornerRadius::same(R_CTRL);

            w.hovered.bg_fill = BG_HOVER;
            w.hovered.weak_bg_fill = BG_HOVER;
            w.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
            w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
            w.hovered.corner_radius = CornerRadius::same(R_CTRL);

            w.active.bg_fill = Color32::from_rgb(0xDD, 0xE3, 0xEB);
            w.active.weak_bg_fill = Color32::from_rgb(0xDD, 0xE3, 0xEB);
            w.active.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
            w.active.fg_stroke = Stroke::new(1.0, TEXT);
            w.active.corner_radius = CornerRadius::same(R_CTRL);

            w.open.bg_fill = Color32::from_rgb(0xE6, 0xEA, 0xF0);
            w.open.weak_bg_fill = Color32::from_rgb(0xE6, 0xEA, 0xF0);
            w.open.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
            w.open.fg_stroke = Stroke::new(1.0, TEXT);
            w.open.corner_radius = CornerRadius::same(R_CTRL);
        }

        style.visuals = v;

        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.window_margin = Margin::same(12);
        style.spacing.menu_margin = Margin::same(8);
        style.spacing.interact_size.y = 26.0;

        style.text_styles = [
            (egui::TextStyle::Heading, egui::FontId::new(17.0, egui::FontFamily::Proportional)),
            (egui::TextStyle::Body, egui::FontId::new(14.0, egui::FontFamily::Proportional)),
            (egui::TextStyle::Button, egui::FontId::new(14.0, egui::FontFamily::Proportional)),
            (egui::TextStyle::Small, egui::FontId::new(11.5, egui::FontFamily::Proportional)),
            (egui::TextStyle::Monospace, egui::FontId::new(12.0, egui::FontFamily::Monospace)),
        ]
        .into();
    });
}

// ---------------------------------------------------------------- 组件

/// 卡片容器。返回内容闭包的返回值。
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_rect(ui, add).0
}

/// 卡片容器，额外返回卡片矩形（用来在外面画左侧色条）。
pub fn card_rect<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> (R, egui::Rect) {
    let inner = Frame::NONE
        .fill(BG_CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(R_CARD))
        .inner_margin(Margin::same(PAD_CARD))
        .shadow(Shadow {
            offset: [0, 1],
            blur: 4,
            spread: 0,
            color: Color32::from_black_alpha(12),
        })
        .show(ui, add);
    (inner.inner, inner.response.rect)
}

/// 卡片标题：左侧短色条 + 标题字
pub fn card_title(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(3.0, 15.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(1), ACCENT);
        ui.label(RichText::new(text).size(14.0).color(TEXT).strong());
    });
}

/// 警示框：浅黄底 + 橙边。用来放「动手前你必须先知道」的风险说明。
pub fn warn_box<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::NONE
        .fill(Color32::from_rgb(0xFF, 0xF8, 0xE6))
        .stroke(Stroke::new(1.0, alpha(WARN, 90)))
        .corner_radius(CornerRadius::same(R_CTRL))
        .inner_margin(Margin::same(10))
        .show(ui, add)
        .inner
}

/// 圆角胶囊徽章
pub fn badge(ui: &mut Ui, text: &str, color: Color32) {
    Frame::NONE
        .fill(alpha(color, 26))
        .stroke(Stroke::new(1.0, alpha(color, 80)))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.5).color(color).strong());
        });
}

fn button_text(text: &str, color: Color32, size: f32, strong: bool) -> RichText {
    let t = RichText::new(text).size(size).color(color);
    if strong {
        t.strong()
    } else {
        t
    }
}

/// 主操作按钮：强调色填充
pub fn primary_button(ui: &mut Ui, text: &str, enabled: bool) -> egui::Response {
    let btn = egui::Button::new(button_text(
        text,
        if enabled { ACCENT_TEXT } else { alpha(TEXT_MUTED, 150) },
        14.0,
        true,
    ))
    .fill(if enabled { ACCENT } else { Color32::from_rgb(0xE6, 0xE9, 0xED) })
    .stroke(Stroke::new(
        1.0,
        if enabled { ACCENT_BORDER } else { BORDER },
    ))
    .corner_radius(CornerRadius::same(R_CTRL));
    ui.add_enabled(enabled, btn)
}

/// 次操作按钮：白底描边
pub fn ghost_button(ui: &mut Ui, text: &str, enabled: bool) -> egui::Response {
    let btn = egui::Button::new(button_text(
        text,
        if enabled { TEXT } else { alpha(TEXT_MUTED, 150) },
        13.0,
        false,
    ))
    .fill(BG_CARD)
    .stroke(Stroke::new(1.0, BORDER_STRONG))
    .corner_radius(CornerRadius::same(R_CTRL));
    ui.add_enabled(enabled, btn)
}

/// 危险操作按钮：描边红
pub fn danger_button(ui: &mut Ui, text: &str, enabled: bool) -> egui::Response {
    let btn = egui::Button::new(button_text(
        text,
        if enabled { DANGER } else { alpha(TEXT_MUTED, 150) },
        13.0,
        false,
    ))
    .fill(BG_CARD)
    .stroke(Stroke::new(1.0, if enabled { alpha(DANGER, 140) } else { BORDER }))
    .corner_radius(CornerRadius::same(R_CTRL));
    ui.add_enabled(enabled, btn)
}

/// 小字辅助文本
pub fn hint(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).size(11.5).color(TEXT_MUTED)
}

/// 等宽小字（路径展示）
pub fn path_text(text: impl Into<String>) -> RichText {
    RichText::new(text.into())
        .size(11.5)
        .monospace()
        .color(TEXT_MUTED)
}
