//! 卡片、面板、贴边竖条、设置面板的布局与绘制。全部是 DIP 坐标。

use crate::gfx::{rgba, Align, Target};
use crate::icon;
use crate::net::ProcSample;

pub const CARD_W: f32 = 96.0;
pub const CARD_H: f32 = 52.0;
pub const CARD_MARGIN: f32 = 6.0;

pub const PANEL_W: f32 = 300.0;
pub const PANEL_H: f32 = 278.0;
pub const PANEL_MARGIN: f32 = 10.0;

pub const DOCK_W: f32 = 16.0;
pub const DOCK_H: f32 = 48.0;

pub const SETTINGS_W: f32 = 400.0;
pub const SETTINGS_H: f32 = 384.0;
pub const SETTINGS_MARGIN: f32 = 14.0;

const TEXT: u8 = 32;
const MUTED: u8 = 140;
const UP: (u8, u8, u8) = (255, 160, 38);
const DOWN: (u8, u8, u8) = (52, 199, 89);

pub fn fmt_speed(bps: f64) -> String {
    let kb = bps / 1024.0;
    if kb < 0.05 {
        "0.0 KB/s".to_string()
    } else if kb < 1024.0 {
        format!("{kb:.1} KB/s")
    } else {
        format!("{:.1} MB/s", kb / 1024.0)
    }
}

fn shadow(t: &Target, x: f32, y: f32, w: f32, h: f32, r: f32, layers: u32, strength: u8) {
    for i in 0..layers {
        let pad = (layers - i) as f32;
        let a = (strength / (i + 1) as u8).max(2);
        t.fill_round(x - pad, y - pad + 1.0, w + pad * 2.0, h + pad * 2.0, r + pad, rgba(0, 0, 0, a));
    }
}

fn card_body(t: &Target, x: f32, y: f32, w: f32, h: f32, radius: f32) {
    shadow(t, x, y, w, h, radius, 3, 6);
    t.fill_round(x, y, w, h, radius, rgba(255, 255, 255, 255));
    t.stroke_round(x + 0.5, y + 0.5, w - 1.0, h - 1.0, radius, rgba(0, 0, 0, 16), 1.0);
}

fn card_rows(t: &Target, cx: f32, cy: f32, up: f64, down: f64) {
    let rows = [(up, true, UP), (down, false, DOWN)];
    for (i, (v, is_up, c)) in rows.iter().enumerate() {
        let ry = cy + 7.0 + i as f32 * 20.0;
        t.arrow(cx + 9.0, ry + 2.0, 12.0, *is_up, rgba(c.0, c.1, c.2, 255));
        t.text(
            &fmt_speed(*v),
            cx + 25.0,
            ry,
            CARD_W - 28.0,
            18.0,
            12.0,
            rgba(TEXT, TEXT, TEXT, 255),
            Align::Left,
        );
    }
}

/// 小卡片：两行，上行橙、下行绿。
pub fn draw_card(t: &Target, up: f64, down: f64, radius: f32) {
    t.clear();
    let (x, y) = (CARD_MARGIN, CARD_MARGIN);
    card_body(t, x, y, CARD_W, CARD_H, radius);
    card_rows(t, x, y, up, down);
}

/// 一段竖直虚线：只有当前速度所在的那一格上色，其余是浅灰（火绒的做法）。
fn dash_half(t: &Target, x: f32, w: f32, y_top: f32, h: f32, v: f64, c: (u8, u8, u8)) {
    let dashes = 6u32;
    let dash_h = 1.6;
    let gap = ((h - dashes as f32 * dash_h) / (dashes - 1) as f32).max(1.0);
    let lvl = level(v);
    let mark = (lvl * (dashes - 1) as f64).round() as u32;
    for i in 0..dashes {
        let dy = y_top + (dashes - 1 - i) as f32 * (dash_h + gap);
        let col = if i == mark {
            rgba(c.0, c.1, c.2, 255)
        } else {
            rgba(212, 212, 212, 255)
        };
        t.fill_round(x, dy, w, dash_h, 1.0, col);
    }
}

fn level(bps: f64) -> f64 {
    let v = (1.0f64 + bps / 1024.0).ln() / (1.0f64 + 1024.0).ln();
    v.clamp(0.0, 1.0)
}

/// 贴屏幕边缘的那一侧做成直角，边框只留在内侧。
fn dock_body(t: &Target, x: f32, y: f32, w: f32, h: f32, r: f32, dock_right: bool) {
    let shadow_x = if dock_right { x - 2.0 } else { x + 2.0 };
    t.fill_round(shadow_x, y - 1.0, w + 2.0, h + 2.0, r + 1.0, rgba(0, 0, 0, 7));
    t.fill_round(x, y, w, h, r, rgba(255, 255, 255, 255));
    t.stroke_round(x + 0.5, y + 0.5, w - 1.0, h - 1.0, r, rgba(0, 0, 0, 20), 1.0);

    let cw = r + 1.0;
    let line = rgba(0, 0, 0, 20);
    if dock_right {
        t.fill_rect(x + w - cw, y - 1.0, cw, h + 2.0, rgba(255, 255, 255, 255));
        t.fill_rect(x + w - cw, y + 0.5, cw, 1.0, line);
        t.fill_rect(x + w - cw, y + h - 1.5, cw, 1.0, line);
    } else {
        t.fill_rect(x, y - 1.0, cw, h + 2.0, rgba(255, 255, 255, 255));
        t.fill_rect(x, y + 0.5, cw, 1.0, line);
        t.fill_rect(x, y + h - 1.5, cw, 1.0, line);
    }
}

/// 贴边竖条；`p` 是展开进度（0 = 竖条，1 = 完整卡片）。
pub fn draw_dock(t: &Target, up: f64, down: f64, dock_right: bool, radius: f32, p: f32) {
    t.clear();
    let win_w = CARD_W + CARD_MARGIN * 2.0;
    let p = p.clamp(0.0, 1.0);
    let w = DOCK_W + (CARD_W - DOCK_W) * p;
    let x = if dock_right { win_w - w } else { 0.0 };
    let y = CARD_MARGIN;
    let r = radius.min(w / 2.0);

    if p < 0.5 {
        dock_body(t, x, y, w, CARD_H, r, dock_right);
    } else {
        card_body(t, x, y, w, CARD_H, r);
    }

    if p >= 0.5 {
        if p > 0.55 {
            card_rows(t, x + w - CARD_W, y, up, down);
        }
        return;
    }
    let strip_x = if dock_right { x + w - DOCK_W } else { x };
    let strip_y = y + (CARD_H - DOCK_H) / 2.0;
    // 上半段是上传，下半段是下载
    let half_h = (DOCK_H - 10.0) / 2.0;
    dash_half(t, strip_x + 5.5, 5.0, strip_y + 3.0, half_h, up, UP);
    dash_half(t, strip_x + 5.5, 5.0, strip_y + 7.0 + half_h, half_h, down, DOWN);
}

fn section_header(t: &Target, x: f32, y: f32, w: f32, label: &str, radius: f32) {
    t.fill_round(x, y, w, 26.0, radius * 0.6, rgba(242, 242, 242, 255));
    t.text(label, x + 8.0, y, w - 16.0, 26.0, 12.0, rgba(58, 58, 58, 255), Align::Left);
}

fn proc_row(t: &Target, x: f32, y: f32, w: f32, p: &ProcSample, is_up: bool) {
    let c = if is_up { UP } else { DOWN };
    let v = if is_up { p.up } else { p.down };
    let active = v >= 1.0;

    match icon::get(&p.path) {
        Some(ic) => t.draw_icon(&ic, x + 2.0, y + 7.0, 16.0),
        None => {
            let (c, _) = hsl_to_rgb(name_hue(&p.name), 0.32, 0.62);
            t.fill_round(x + 2.0, y + 7.0, 16.0, 16.0, 4.0, rgba(c.0, c.1, c.2, 255));
            t.fill_round(x + 5.0, y + 11.0, 10.0, 2.0, 1.0, rgba(255, 255, 255, 210));
            t.fill_round(x + 5.0, y + 15.0, 6.0, 2.0, 1.0, rgba(255, 255, 255, 160));
        }
    }

    let name = t.ellipsize(&p.name, 12.0, 118.0);
    let name_color = if active { rgba(TEXT, TEXT, TEXT, 255) } else { rgba(135, 135, 135, 255) };
    t.text(&name, x + 26.0, y, 122.0, 30.0, 12.0, name_color, Align::Left);

    t.arrow(x + w - 78.0, y + 10.0, 10.0, is_up, rgba(c.0, c.1, c.2, 255));
    t.text(&fmt_speed(v), x + w - 64.0, y, 64.0, 30.0, 12.0, rgba(c.0, c.1, c.2, 255), Align::Right);
}

fn name_hue(name: &str) -> f32 {
    let mut v: u32 = 7;
    for b in name.bytes() {
        v = v.wrapping_mul(31).wrapping_add(b as u32);
    }
    (v % 360) as f32
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> ((u8, u8, u8), bool) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let f = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    ((f(r1), f(g1), f(b1)), true)
}

pub fn draw_panel(
    t: &Target,
    up: f64,
    down: f64,
    procs_up: &[ProcSample],
    procs_down: &[ProcSample],
    radius: f32,
    limited: bool,
) {
    t.clear();
    let x = PANEL_MARGIN;
    let y = PANEL_MARGIN;
    let w = PANEL_W - PANEL_MARGIN * 2.0;
    let h = PANEL_H - PANEL_MARGIN * 2.0;
    let r = radius + 2.0;
    shadow(t, x, y, w, h, r, 4, 7);
    t.fill_round(x, y, w, h, r, rgba(255, 255, 255, 255));
    t.stroke_round(x + 0.5, y + 0.5, w - 1.0, h - 1.0, r, rgba(0, 0, 0, 18), 1.0);

    if limited {
        t.text(
            "需要管理员权限才能显示进程榜",
            x + 8.0,
            y + 100.0,
            w - 16.0,
            30.0,
            13.0,
            rgba(150, 150, 150, 255),
            Align::Center,
        );
        return;
    }

    let mut cy = y + 8.0;
    for (is_up, rows, total) in [(true, procs_up, up), (false, procs_down, down)] {
        let (c, label) = if is_up { (UP, "实时上传") } else { (DOWN, "实时下载") };
        section_header(t, x + 8.0, cy, w - 16.0, label, radius);
        t.text(
            &fmt_speed(total),
            x + w - 16.0 - 92.0,
            cy,
            92.0,
            26.0,
            12.0,
            rgba(c.0, c.1, c.2, 255),
            Align::Right,
        );
        cy += 26.0;
        let mut sorted: Vec<&ProcSample> = rows.iter().collect();
        sorted.sort_by(|a, b| {
            let va = if is_up { a.up } else { a.down };
            let vb = if is_up { b.up } else { b.down };
            vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
        });
        for p in sorted.iter().take(3) {
            proc_row(t, x + 8.0, cy, w - 16.0, p, is_up);
            cy += 30.0;
        }
        for _ in sorted.len().min(3)..3 {
            cy += 30.0;
        }
        cy += 10.0;
    }
}

// ---------- 设置面板 ----------

#[derive(Clone, Copy)]
pub struct SLayout {
    pub close: (f32, f32, f32, f32),
    pub row_toggle: (f32, f32, f32, f32),
    pub row_opacity: (f32, f32, f32, f32),
    pub row_radius: (f32, f32, f32, f32),
    pub toggle: (f32, f32, f32, f32),
    pub track: (f32, f32, f32, f32),
    pub reset: (f32, f32, f32, f32),
}

pub fn settings_layout() -> SLayout {
    SLayout {
        close: (342.0, 24.0, 32.0, 32.0),
        row_toggle: (20.0, 70.0, 360.0, 64.0),
        row_opacity: (20.0, 134.0, 360.0, 88.0),
        row_radius: (20.0, 222.0, 360.0, 88.0),
        toggle: (322.0, 88.0, 44.0, 26.0),
        track: (34.0, 188.0, 312.0, 6.0),
        reset: (270.0, 312.0, 96.0, 32.0),
    }
}

fn inside(r: (f32, f32, f32, f32), x: f32, y: f32) -> bool {
    x >= r.0 && x <= r.0 + r.2 && y >= r.1 && y <= r.1 + r.3
}

/// 设置面板的行号：用于悬停高亮
pub fn settings_row_at(x: f32, y: f32) -> u8 {
    let l = settings_layout();
    if inside(l.row_toggle, x, y) {
        1
    } else if inside(l.row_opacity, x, y) {
        2
    } else if inside(l.row_radius, x, y) {
        3
    } else if inside(l.reset, x, y) {
        4
    } else {
        0
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum SettingHit {
    None,
    Close,
    Toggle,
    Opacity,
    Radius,
    Reset,
}

pub fn settings_hit(x: f32, y: f32) -> SettingHit {
    let l = settings_layout();
    if inside(l.close, x, y) {
        SettingHit::Close
    } else if inside(l.row_toggle, x, y) {
        SettingHit::Toggle
    } else if inside(l.row_opacity, x, y) {
        SettingHit::Opacity
    } else if inside(l.row_radius, x, y) {
        SettingHit::Radius
    } else if inside(l.reset, x, y) {
        SettingHit::Reset
    } else {
        SettingHit::None
    }
}

fn track_frac(x: f32) -> f32 {
    let l = settings_layout();
    ((x - l.track.0) / l.track.2).clamp(0.0, 1.0)
}

pub fn opacity_from_x(x: f32) -> u8 {
    (80.0 + track_frac(x) * 175.0).round() as u8
}

pub fn radius_from_x(x: f32) -> f32 {
    (track_frac(x) * 16.0).round()
}

fn divider(t: &Target, y: f32) {
    t.fill_rect(34.0, y, 332.0, 1.0, rgba(0, 0, 0, 12));
}

fn slider(t: &Target, y: f32, frac: f32, accent: (u8, u8, u8)) {
    let l = settings_layout();
    let (tx, _, tw, th) = l.track;
    t.fill_round(tx, y, tw, th, th / 2.0, rgba(233, 233, 233, 255));
    t.fill_round(tx, y, tw * frac, th, th / 2.0, rgba(accent.0, accent.1, accent.2, 255));
    let kx = tx + tw * frac;
    let ky = y + th / 2.0;
    t.fill_circle(kx, ky, 9.0, rgba(0, 0, 0, 18));
    t.fill_circle(kx, ky - 0.5, 8.5, rgba(255, 255, 255, 255));
    t.stroke_circle(kx, ky - 0.5, 8.5, rgba(0, 0, 0, 28), 1.0);
    t.fill_circle(kx, ky - 0.5, 3.5, rgba(accent.0, accent.1, accent.2, 255));
}

fn row_bg(t: &Target, hover: bool, r: (f32, f32, f32, f32)) {
    if hover {
        t.fill_round(r.0, r.1 + 2.0, r.2, r.3 - 4.0, 10.0, rgba(246, 246, 246, 255));
    }
}

pub fn draw_settings(t: &Target, autostart: bool, opacity: u8, radius: f32, hover: u8) {
    t.clear();
    let l = settings_layout();
    let (x, y) = (SETTINGS_MARGIN, SETTINGS_MARGIN);
    let w = SETTINGS_W - SETTINGS_MARGIN * 2.0;
    let h = SETTINGS_H - SETTINGS_MARGIN * 2.0;
    let r = 12.0;
    shadow(t, x, y, w, h, r, 4, 9);
    t.fill_round(x, y, w, h, r, rgba(255, 255, 255, 255));
    t.stroke_round(x + 0.5, y + 0.5, w - 1.0, h - 1.0, r, rgba(0, 0, 0, 18), 1.0);

    t.text_bold("设置", 34.0, 26.0, 200.0, 32.0, 16.0, rgba(24, 24, 24, 255), Align::Left);
    t.text("×", l.close.0, l.close.1, l.close.2, l.close.3, 17.0, rgba(130, 130, 130, 255), Align::Center);
    divider(t, 70.0);

    // 开机启动
    row_bg(t, hover == 1, l.row_toggle);
    t.text("开机启动", 34.0, 82.0, 240.0, 26.0, 13.0, rgba(TEXT, TEXT, TEXT, 255), Align::Left);
    t.text(
        "登录后自动运行，静默启动不弹 UAC",
        34.0,
        106.0,
        260.0,
        20.0,
        11.0,
        rgba(MUTED, MUTED, MUTED, 255),
        Align::Left,
    );
    let (tx, ty, tw, th) = l.toggle;
    t.fill_round(tx, ty, tw, th, th / 2.0, if autostart { rgba(52, 199, 89, 255) } else { rgba(225, 225, 225, 255) });
    let kx = if autostart { tx + tw - th / 2.0 } else { tx + th / 2.0 };
    t.fill_circle(kx, ty + th / 2.0, th / 2.0 - 2.0, rgba(255, 255, 255, 255));
    divider(t, 134.0);

    // 透明度
    row_bg(t, hover == 2, l.row_opacity);
    t.text("透明度", 34.0, 146.0, 200.0, 26.0, 13.0, rgba(TEXT, TEXT, TEXT, 255), Align::Left);
    t.text(
        &format!("{}%", ((opacity as f32 / 255.0) * 100.0).round() as i32),
        296.0,
        146.0,
        70.0,
        26.0,
        13.0,
        rgba(120, 120, 120, 255),
        Align::Right,
    );
    slider(t, 186.0, ((opacity as f32 - 80.0) / 175.0).clamp(0.0, 1.0), UP);
    divider(t, 222.0);

    // 圆角
    row_bg(t, hover == 3, l.row_radius);
    t.text("圆角", 34.0, 234.0, 200.0, 26.0, 13.0, rgba(TEXT, TEXT, TEXT, 255), Align::Left);
    t.text(&format!("{:.0}", radius), 296.0, 234.0, 70.0, 26.0, 13.0, rgba(120, 120, 120, 255), Align::Right);
    slider(t, 274.0, (radius / 16.0).clamp(0.0, 1.0), DOWN);

    // 底部
    t.text(
        "拖动滑块即时生效，设置自动保存",
        34.0,
        320.0,
        220.0,
        20.0,
        11.0,
        rgba(MUTED, MUTED, MUTED, 255),
        Align::Left,
    );
    let (rx, ry, rw, rh) = l.reset;
    t.fill_round(rx, ry, rw, rh, 8.0, if hover == 4 { rgba(238, 238, 238, 255) } else { rgba(245, 245, 245, 255) });
    t.text("恢复默认", rx, ry, rw, rh, 12.0, rgba(70, 70, 70, 255), Align::Center);
}
