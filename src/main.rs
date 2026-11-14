//! FlowDock 流量浮窗 —— 入口、三个分层窗口的生命周期、悬停状态机、贴边吸附、设置面板、托盘。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod etw;
mod gfx;
mod icon;
mod net;
mod tray;
mod ui;
mod win32;

use std::cell::RefCell;
use std::sync::Arc;

use windows::core::{w, PCWSTR, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use config::{Config, Dock};
use gfx::{Renderer, Target};
use net::Sampler;
use ui::SettingHit;
use win32::{Surface, WM_TRAY};

const CARD_CLASS: PCWSTR = w!("FlowDockCard");
const PANEL_CLASS: PCWSTR = w!("FlowDockPanel");
const SETTINGS_CLASS: PCWSTR = w!("FlowDockSettings");

const WM_MOUSELEAVE_MSG: u32 = 0x02A3;
const WM_OPEN_SETTINGS: u32 = WM_APP + 6;

const TIMER_SHOW: usize = 1;
const TIMER_HIDE: usize = 2;
const TIMER_FADE: usize = 3;
const TIMER_REFRESH: usize = 4;
const TIMER_EXPAND: usize = 5;

thread_local! {
    static APP: RefCell<Option<Box<App>>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        let mut b = cell.try_borrow_mut().ok()?;
        b.as_mut().map(|app| f(app))
    })
}

struct Win {
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    surface: Surface,
    target: Target,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Card,
    Panel,
    Settings,
}

struct App {
    card: Win,
    panel: Win,
    settings: Win,
    sampler: Arc<Sampler>,
    cfg: Config,
    dpi: f32,
    panel_shown: bool,
    panel_alpha: f32,
    panel_target: f32,
    settings_shown: bool,
    mouse_card: bool,
    mouse_panel: bool,
    tracking_card: bool,
    tracking_panel: bool,
    dragging: bool,
    drag_cursor: (i32, i32),
    drag_win: (i32, i32),
    slider: Option<SettingHit>,
    settings_hover: u8,
    expand: f32,
    expand_target: f32,
    pinned: bool,
    click_start: (i32, i32),
    tray_icon: HICON,
}

impl App {
    fn scale(&self) -> f32 {
        self.dpi / 96.0
    }

    fn kind(&self, hwnd: HWND) -> Option<Kind> {
        if self.card.hwnd == hwnd {
            Some(Kind::Card)
        } else if self.panel.hwnd == hwnd {
            Some(Kind::Panel)
        } else if self.settings.hwnd == hwnd {
            Some(Kind::Settings)
        } else {
            None
        }
    }

    fn surface_for(&self, hwnd: HWND) -> Option<&Surface> {
        match self.kind(hwnd)? {
            Kind::Card => Some(&self.card.surface),
            Kind::Panel => Some(&self.panel.surface),
            Kind::Settings => Some(&self.settings.surface),
        }
    }

    fn draw_card(&self) {
        let (up, down) = self.sampler.nic_totals();
        self.card.target.begin();
        match self.cfg.dock {
            Dock::None => ui::draw_card(&self.card.target, up, down, self.cfg.radius),
            Dock::Right => ui::draw_dock(&self.card.target, up, down, true, self.cfg.radius, self.expand),
            Dock::Left => ui::draw_dock(&self.card.target, up, down, false, self.cfg.radius, self.expand),
        }
        self.card.target.end();
        self.card.surface.present(self.card.hwnd, self.card.x, self.card.y, self.cfg.opacity);
    }

    fn draw_panel(&self) {
        let (up, down) = self.sampler.totals();
        let procs = self.sampler.procs();
        self.panel.target.begin();
        ui::draw_panel(&self.panel.target, up, down, &procs, &procs, self.cfg.radius, self.sampler.limited);
        self.panel.target.end();
        let a = (self.cfg.opacity as f32 * self.panel_alpha) as u8;
        self.panel.surface.present(self.panel.hwnd, self.panel.x, self.panel.y, a);
    }

    fn draw_settings(&self) {
        self.settings.target.begin();
        ui::draw_settings(&self.settings.target, self.cfg.autostart, self.cfg.opacity, self.cfg.radius, self.settings_hover);
        self.settings.target.end();
        self.settings
            .surface
            .present(self.settings.hwnd, self.settings.x, self.settings.y, 255);
    }

    /// 卡片的可见矩形（DIP）：贴边时宽度随展开进度变化。
    fn visual(&self) -> (f32, f32, f32, f32) {
        let s = self.scale();
        let cx = self.card.x as f32 / s;
        let cy = self.card.y as f32 / s;
        let win_w = ui::CARD_W + ui::CARD_MARGIN * 2.0;
        let w = ui::DOCK_W + (ui::CARD_W - ui::DOCK_W) * self.expand.clamp(0.0, 1.0);
        match self.cfg.dock {
            Dock::None => (cx + ui::CARD_MARGIN, cy + ui::CARD_MARGIN, ui::CARD_W, ui::CARD_H),
            Dock::Right => (cx + win_w - w, cy + ui::CARD_MARGIN, w, ui::CARD_H),
            Dock::Left => (cx, cy + ui::CARD_MARGIN, w, ui::CARD_H),
        }
    }

    /// 面板位置：普通形态固定在卡片左上方；贴边形态放在展开后卡片的内侧。
    fn panel_pos(&self) -> (i32, i32) {
        let s = self.scale();
        let (vx, vy, vw, _vh) = self.visual();
        let (px, py) = match self.cfg.dock {
            Dock::None | Dock::Right => (
                vx - 8.0 - (ui::PANEL_W - ui::PANEL_MARGIN),
                vy - ui::PANEL_MARGIN,
            ),
            Dock::Left => (
                vx + vw + 8.0 - ui::PANEL_MARGIN,
                vy - ui::PANEL_MARGIN,
            ),
        };
        let mut x = (px * s).round() as i32;
        let mut y = (py * s).round() as i32;
        let wa = win32::work_area_at(self.card.x + self.card.w / 2, self.card.y + self.card.h / 2);
        x = x.clamp(wa.left, (wa.right - self.panel.w).max(wa.left));
        y = y.clamp(wa.top, (wa.bottom - self.panel.h).max(wa.top));
        (x, y)
    }

    fn settings_pos(&self) -> (i32, i32) {
        let wa = win32::work_area_at(self.card.x, self.card.y);
        let x = wa.left + ((wa.right - wa.left - self.settings.w) / 2).max(0);
        let y = wa.top + ((wa.bottom - wa.top - self.settings.h) / 2).max(0);
        (x, y)
    }

    fn show_panel(&mut self) {
        if self.panel_shown {
            // 正在淡出时又被悬停：反向拉回来，别等下一次 mousemove
            if self.panel_target < 1.0 {
                self.panel_target = 1.0;
                unsafe {
                    SetTimer(Some(self.card.hwnd), TIMER_FADE, 15, None);
                }
            }
            return;
        }
        if self.cfg.dock != Dock::None && self.expand_target < 1.0 {
            self.expand_target = 1.0;
            unsafe {
                SetTimer(Some(self.card.hwnd), TIMER_EXPAND, 16, None);
            }
        }
        if self.panel_shown {
            return;
        }
        let (px, py) = self.panel_pos();
        self.panel.x = px;
        self.panel.y = py;
        self.panel_alpha = 0.0;
        self.panel_target = 1.0;
        self.panel_shown = true;
        self.draw_panel();
        unsafe {
            let _ = ShowWindow(self.panel.hwnd, SW_SHOWNOACTIVATE);
            // ShowWindow 不保证面板回到置顶带顶部：被别的窗口压住时就是“悬停没反应”
            let _ = SetWindowPos(
                self.panel.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            SetTimer(Some(self.card.hwnd), TIMER_FADE, 15, None);
        }
    }

    fn hide_panel(&mut self) {
        if !self.panel_shown {
            return;
        }
        self.panel_target = 0.0;
        unsafe {
            SetTimer(Some(self.card.hwnd), TIMER_FADE, 15, None);
        }
    }

    /// 展开/收起的补间动画。
    fn tick_expand(&mut self) -> bool {
        let t = self.expand_target;
        let e = self.expand;
        let ne = if (t - e).abs() < 0.02 { t } else { e + (t - e) * 0.35 };
        self.expand = ne;
        self.draw_card();
        if self.panel_shown {
            let (px, py) = self.panel_pos();
            self.panel.x = px;
            self.panel.y = py;
            let a = (self.cfg.opacity as f32 * self.panel_alpha) as u8;
            self.panel.surface.present(self.panel.hwnd, px, py, a);
        }
        (t - ne).abs() > 0.01
    }

    fn toggle_pin(&mut self) {
        self.pinned = !self.pinned;
        if self.pinned {
            if self.cfg.dock != Dock::None {
                self.expand_target = 1.0;
                unsafe {
                    SetTimer(Some(self.card.hwnd), TIMER_EXPAND, 16, None);
                }
            }
            self.show_panel();
        } else {
            self.hide_panel();
            if self.cfg.dock != Dock::None {
                self.expand_target = 0.0;
                unsafe {
                    SetTimer(Some(self.card.hwnd), TIMER_EXPAND, 16, None);
                }
            }
        }
    }

    /// 返回 true 表示还需要继续动画。
    fn tick_fade(&mut self) -> bool {
        let target = self.panel_target;
        let a = self.panel_alpha;
        let na = if (target - a).abs() < 0.02 { target } else { a + (target - a) * 0.3 };
        self.panel_alpha = na;
        if target <= 0.0 && na <= 0.01 {
            self.panel_shown = false;
            unsafe {
                let _ = ShowWindow(self.panel.hwnd, SW_HIDE);
            }
            return false;
        }
        let a = (self.cfg.opacity as f32 * na) as u8;
        self.panel.surface.present(self.panel.hwnd, self.panel.x, self.panel.y, a);
        (target - na).abs() > 0.01
    }

    fn drag_to(&mut self, cx: i32, cy: i32) {
        let nx = self.drag_win.0 + (cx - self.drag_cursor.0);
        let ny = self.drag_win.1 + (cy - self.drag_cursor.1);
        self.card.x = nx;
        self.card.y = ny;

        // 从贴边状态往内拖够远就恢复卡片形态
        let mut dock_changed = false;
        if self.cfg.dock != Dock::None {
            let wa = win32::work_area_at(cx, cy);
            let edge = (24.0 * self.scale()) as i32;
            let far_from_right = wa.right - (nx + self.card.w) > edge;
            let far_from_left = nx - wa.left > edge;
            if far_from_right && far_from_left {
                self.cfg.dock = Dock::None;
                dock_changed = true;
            }
        }
        if dock_changed {
            self.draw_card();
        } else {
            self.card.surface.present(self.card.hwnd, nx, ny, self.cfg.opacity);
        }

        if self.panel_shown {
            let (px, py) = self.panel_pos();
            self.panel.x = px;
            self.panel.y = py;
            let a = (self.cfg.opacity as f32 * self.panel_alpha) as u8;
            self.panel.surface.present(self.panel.hwnd, px, py, a);
        }
    }

    fn end_drag(&mut self) {
        self.dragging = false;
        let dock_before = self.cfg.dock;
        unsafe {
            let _ = ReleaseCapture();
        }
        let wa = win32::work_area_at(self.card.x + self.card.w / 2, self.card.y + self.card.h / 2);
        let edge = (20.0 * self.scale()) as i32;
        let right_gap = wa.right - (self.card.x + self.card.w);
        let left_gap = self.card.x - wa.left;

        if right_gap <= edge {
            self.cfg.dock = Dock::Right;
            self.card.x = wa.right - self.card.w;
        } else if left_gap <= edge {
            self.cfg.dock = Dock::Left;
            self.card.x = wa.left;
        } else {
            self.cfg.dock = Dock::None;
            self.expand = 0.0;
            self.expand_target = 0.0;
            self.pinned = false;
        }
        self.card.y = self.card.y.clamp(wa.top, (wa.bottom - self.card.h).max(wa.top));
        // 只是点了一下、位置没变就不要写磁盘
        let moved = self.cfg.x != Some(self.card.x) || self.cfg.y != Some(self.card.y);
        let dock_changed = self.cfg.dock != dock_before;
        self.cfg.x = Some(self.card.x);
        self.cfg.y = Some(self.card.y);
        if moved || dock_changed {
            self.cfg.save();
        }
        self.draw_card();
        if self.panel_shown {
            let (px, py) = self.panel_pos();
            self.panel.x = px;
            self.panel.y = py;
        }
    }

    fn toggle(&mut self) {
        unsafe {
            if IsWindowVisible(self.card.hwnd).as_bool() {
                let _ = ShowWindow(self.card.hwnd, SW_HIDE);
                self.pinned = false;
                self.hide_panel();
            } else {
                let _ = ShowWindow(self.card.hwnd, SW_SHOWNOACTIVATE);
                self.draw_card();
            }
        }
    }

    fn open_settings(&mut self) {
        let (x, y) = self.settings_pos();
        self.settings.x = x;
        self.settings.y = y;
        self.settings_shown = true;
        self.draw_settings();
        unsafe {
            let _ = ShowWindow(self.settings.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                self.settings.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    fn close_settings(&mut self) {
        self.settings_shown = false;
        self.slider = None;
        unsafe {
            let _ = ShowWindow(self.settings.hwnd, SW_HIDE);
        }
    }

    fn toggle_autostart(&mut self) {
        let want = !self.cfg.autostart;
        let ok = config::apply_autostart(want);
        if ok {
            self.cfg.autostart = want;
        } else {
            gfx::log("apply_autostart failed (需要管理员权限？)");
        }
        self.cfg.save();
        self.draw_settings();
    }

    fn settings_dip(&self, cx: i32, cy: i32) -> (f32, f32) {
        let s = self.scale();
        (((cx - self.settings.x) as f32) / s, ((cy - self.settings.y) as f32) / s)
    }

    fn slider_to(&mut self, cx: i32) {
        let s = self.scale();
        let dx = (cx - self.settings.x) as f32 / s;
        match self.slider {
            Some(SettingHit::Opacity) => {
                self.cfg.opacity = ui::opacity_from_x(dx);
                self.draw_card();
                if self.panel_shown {
                    self.draw_panel();
                }
            }
            Some(SettingHit::Radius) => {
                self.cfg.radius = ui::radius_from_x(dx);
                self.draw_card();
                if self.panel_shown {
                    self.draw_panel();
                }
            }
            _ => {}
        }
        self.draw_settings();
    }
}

fn track_leave(hwnd: HWND) -> bool {
    unsafe {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: u32::MAX,
        };
        TrackMouseEvent(&mut tme).is_ok()
    }
}

fn tray_menu(hwnd: HWND, at: (i32, i32)) -> u32 {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let _ = AppendMenuW(menu, MF_STRING, 1, w!("显示 / 隐藏悬浮窗"));
        let _ = AppendMenuW(menu, MF_STRING, 2, w!("设置…"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, 3, w!("退出"));
        let (cx, cy) = at;
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, cx, cy, Some(0), hwnd, None).0 as u32;
        let _ = DestroyMenu(menu);
        // 菜单关闭后补一条空消息，否则点别处时菜单可能不消失
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        cmd
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let kind = with_app(|a| a.kind(hwnd)).flatten();
    match msg {
        WM_NCHITTEST => {
            let (cx, cy) = win32::cursor_pos();
            let mut rc = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rc);
            let alpha = with_app(|a| {
                a.surface_for(hwnd).map(|s| s.alpha_at(cx - rc.left, cy - rc.top))
            })
            .flatten()
            .unwrap_or(255);
            if alpha < 24 {
                return LRESULT(HTTRANSPARENT as isize);
            }
            return LRESULT(HTCLIENT as isize);
        }
        WM_MOUSEMOVE => {
            let (cx, cy) = win32::cursor_pos();
            match kind {
                Some(Kind::Card) => {
                    with_app(|a| {
                        if a.dragging {
                            a.drag_to(cx, cy);
                            return;
                        }
                        a.mouse_card = true;
                        if a.cfg.dock != Dock::None && a.expand_target < 1.0 {
                            a.expand_target = 1.0;
                            SetTimer(Some(hwnd), TIMER_EXPAND, 16, None);
                        }
                        if !a.panel_shown {
                            SetTimer(Some(hwnd), TIMER_SHOW, 130, None);
                        }
                        if !a.tracking_card {
                            a.tracking_card = track_leave(hwnd);
                        }
                    });
                }
                Some(Kind::Panel) => {
                    with_app(|a| {
                        a.mouse_panel = true;
                        let _ = KillTimer(Some(a.card.hwnd), TIMER_HIDE);
                        if !a.tracking_panel {
                            a.tracking_panel = track_leave(hwnd);
                        }
                    });
                }
                Some(Kind::Settings) => {
                    with_app(|a| {
                        let (dx, dy) = a.settings_dip(cx, cy);
                        let row = ui::settings_row_at(dx, dy);
                        if row != a.settings_hover {
                            a.settings_hover = row;
                            if a.settings_shown && a.slider.is_none() {
                                a.draw_settings();
                            }
                        }
                        if a.slider.is_some() {
                            a.slider_to(cx);
                        }
                    });
                }
                None => {}
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE_MSG => {
            with_app(|a| {
                match kind {
                    Some(Kind::Card) => {
                        a.mouse_card = false;
                        a.tracking_card = false;
                    }
                    Some(Kind::Panel) => {
                        a.mouse_panel = false;
                        a.tracking_panel = false;
                    }
                    _ => {}
                }
                if a.panel_shown && !a.mouse_card && !a.mouse_panel {
                    SetTimer(Some(a.card.hwnd), TIMER_HIDE, 220, None);
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (cx, cy) = win32::cursor_pos();
            match kind {
                Some(Kind::Card) => {
                    with_app(|a| {
                        a.dragging = true;
                        a.click_start = (cx, cy);
                        a.drag_cursor = (cx, cy);
                        a.drag_win = (a.card.x, a.card.y);
                        SetCapture(hwnd);
                    });
                }
                Some(Kind::Settings) => {
                    with_app(|a| {
                        let (dx, dy) = a.settings_dip(cx, cy);
                        match ui::settings_hit(dx, dy) {
                            SettingHit::Close => a.close_settings(),
                            SettingHit::Toggle => a.toggle_autostart(),
                            SettingHit::Reset => {
                                a.cfg.radius = 6.0;
                                a.cfg.opacity = 252;
                                a.cfg.save();
                                a.draw_card();
                                a.draw_settings();
                            }
                            h @ (SettingHit::Opacity | SettingHit::Radius) => {
                                a.slider = Some(h);
                                SetCapture(hwnd);
                                a.slider_to(cx);
                            }
                            SettingHit::None => {}
                        }
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (cx, cy) = win32::cursor_pos();
            with_app(|a| {
                let was_click = a.dragging
                    && (cx - a.click_start.0).abs() <= 4
                    && (cy - a.click_start.1).abs() <= 4;
                if a.dragging {
                    a.end_drag();
                    if was_click {
                        a.toggle_pin();
                    }
                }
                if a.slider.is_some() {
                    a.slider = None;
                    let _ = ReleaseCapture();
                    a.cfg.save();
                }
            });
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            // 火绒也是右键悬浮窗出菜单，托盘右键同样可用。
            // 菜单挂在卡片下沿之外：卡片是 WS_EX_TOPMOST 而菜单不是，
            // 从光标位置弹出会被卡片自己压住，“设置…”根本点不到。
            if kind == Some(Kind::Card) {
                let at = with_app(|a| (a.card.x, a.card.y + a.card.h + 2))
                    .unwrap_or_else(win32::cursor_pos);
                let cmd = tray_menu(hwnd, at);
                match cmd {
                    1 => {
                        with_app(|a| a.toggle());
                    }
                    2 => {
                        with_app(|a| a.open_settings());
                    }
                    3 => unsafe { PostQuitMessage(0) },
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let id = wparam.0;
            match id {
                TIMER_SHOW => with_app(|a| {
                    let _ = KillTimer(Some(a.card.hwnd), TIMER_SHOW);
                    if a.mouse_card && !a.panel_shown {
                        a.show_panel();
                    }
                }),
                TIMER_HIDE => with_app(|a| {
                    let _ = KillTimer(Some(a.card.hwnd), TIMER_HIDE);
                    if a.pinned {
                        return;
                    }
                    if a.panel_shown && !a.mouse_card && !a.mouse_panel {
                        a.hide_panel();
                    }
                    if a.cfg.dock != Dock::None && !a.mouse_card && !a.mouse_panel && a.expand_target > 0.0 {
                        a.expand_target = 0.0;
                        SetTimer(Some(a.card.hwnd), TIMER_EXPAND, 16, None);
                    }
                }),
                TIMER_EXPAND => with_app(|a| {
                    if !a.tick_expand() {
                        let _ = KillTimer(Some(a.card.hwnd), TIMER_EXPAND);
                    }
                }),
                TIMER_FADE => with_app(|a| {
                    if !a.tick_fade() {
                        let _ = KillTimer(Some(a.card.hwnd), TIMER_FADE);
                    }
                }),
                TIMER_REFRESH => with_app(|a| {
                    if IsWindowVisible(a.card.hwnd).as_bool() {
                        a.draw_card();
                    }
                    if a.panel_shown {
                        a.draw_panel();
                    }
                }),
                _ => Some(()),
            };
            LRESULT(0)
        }
        WM_OPEN_SETTINGS => {
            with_app(|a| a.open_settings());
            LRESULT(0)
        }
        WM_TRAY => {
            let event = (lparam.0 as u32) & 0xFFFF;
            match event {
                WM_RBUTTONUP => {
                    let cmd = tray_menu(hwnd, win32::cursor_pos());
                    match cmd {
                        1 => {
                            with_app(|a| a.toggle());
                        }
                        2 => {
                            with_app(|a| a.open_settings());
                        }
                        3 => unsafe { PostQuitMessage(0) },
                        _ => {}
                    }
                }
                WM_LBUTTONDBLCLK => {
                    with_app(|a| a.toggle());
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_PAINT => {
            unsafe {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DESTROY => {
            etw::stop();
            with_app(|a| {
                tray::remove(a.card.hwnd);
                let _ = DestroyIcon(a.tray_icon);
            });
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn register_classes() -> () {
    unsafe {
        let hinst = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(_) => return,
        };
        for class in [CARD_CLASS, PANEL_CLASS, SETTINGS_CLASS] {
            let wc = WNDCLASSW {
                style: WNDCLASS_STYLES(0),
                lpfnWndProc: Some(wndproc),
                hInstance: hinst.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                lpszClassName: class,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                panic!("RegisterClassW failed: {:?}", std::io::Error::last_os_error());
            }
        }
    }
}

fn create_window(class: PCWSTR, w: i32, h: i32, x: i32, y: i32) -> Result<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class,
            w!("FlowDock"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        )
    }
}

/// 把一块分层表面按浅灰底合成后写成 BMP（截图用，不依赖桌面）。
fn save_bmp(s: &Surface, path: &str, bg: (u8, u8, u8)) {
    let (w, h) = (s.w, s.h);
    let row = (w * 4) as usize;
    let data = row * h as usize;
    let mut out: Vec<u8> = Vec::with_capacity(54 + data);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + data) as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(data as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    for y in (0..h).rev() {
        let p = s.row_ptr(y);
        let src = unsafe { std::slice::from_raw_parts(p, row) };
        for x in 0..w as usize {
            let b = src[x * 4] as u32;
            let g = src[x * 4 + 1] as u32;
            let r = src[x * 4 + 2] as u32;
            let a = src[x * 4 + 3] as u32;
            let inv = 255 - a;
            // BMP 的像素顺序是 B,G,R,A
            out.push(((b + bg.2 as u32 * inv / 255).min(255)) as u8);
            out.push(((g + bg.1 as u32 * inv / 255).min(255)) as u8);
            out.push(((r + bg.0 as u32 * inv / 255).min(255)) as u8);
            out.push(255);
        }
    }
    let _ = std::fs::write(path, out);
}

/// --shot：把卡片/面板/竖条/设置渲染成图片，不需要窗口和鼠标。
fn take_shots(dir: &str, scale: f32) -> Result<()> {
    let renderer = Renderer::new()?;
    let dpi = 96.0 * scale;
    let px = |v: f32| (v * scale).round() as i32;
    std::fs::create_dir_all(dir).ok();
    let bg = (247u8, 247u8, 248u8);
    let rows = net::demo_rows();
    let up: f64 = rows.iter().map(|r| r.up).sum();
    let down: f64 = rows.iter().map(|r| r.down).sum();

    let cw = px(ui::CARD_W + ui::CARD_MARGIN * 2.0);
    let ch = px(ui::CARD_H + ui::CARD_MARGIN * 2.0);
    let card = Surface::new(cw, ch);
    let t = renderer.target(card.dc, cw, ch, dpi)?;
    t.begin();
    ui::draw_card(&t, up, down, 6.0);
    t.end();
    save_bmp(&card, &format!("{dir}/card.bmp"), bg);

    let pw = px(ui::PANEL_W);
    let ph = px(ui::PANEL_H);
    let panel = Surface::new(pw, ph);
    let t = renderer.target(panel.dc, pw, ph, dpi)?;
    t.begin();
    ui::draw_panel(&t, up, down, &rows, &rows, 6.0, false);
    t.end();
    save_bmp(&panel, &format!("{dir}/panel.bmp"), bg);

    let sw = px(ui::SETTINGS_W);
    let sh = px(ui::SETTINGS_H);
    let settings = Surface::new(sw, sh);
    let t = renderer.target(settings.dc, sw, sh, dpi)?;
    t.begin();
    ui::draw_settings(&t, false, 252, 6.0, 0);
    t.end();
    save_bmp(&settings, &format!("{dir}/settings.bmp"), bg);

    let dw = px(ui::CARD_W + ui::CARD_MARGIN * 2.0);
    let dh = px(ui::CARD_H + ui::CARD_MARGIN * 2.0);
    let dock = Surface::new(dw, dh);
    let t = renderer.target(dock.dc, dw, dh, dpi)?;
    t.begin();
    ui::draw_dock(&t, up, down, true, 6.0, 0.0);
    t.end();
    save_bmp(&dock, &format!("{dir}/dock.bmp"), bg);
    Ok(())
}

fn main() -> Result<()> {
    if let Some(pos) = std::env::args().position(|a| a == "--shot") {
        let dir = std::env::args().nth(pos + 1).unwrap_or_else(|| "shots".to_string());
        let scale: f32 = std::env::args().nth(pos + 2).and_then(|s| s.parse().ok()).unwrap_or(2.0);
        return take_shots(&dir, scale);
    }

    std::panic::set_hook(Box::new(|info| {
        gfx::log(&format!("panic: {info}"));
    }));

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // 单实例：第二次启动直接退出，避免两个实例抢同一个 ETW 会话
    unsafe {
        let _mutex = windows::Win32::System::Threading::CreateMutexW(None, true, w!("FlowDockSingleInstance"));
        if windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS {
            // 已经在跑：把 --settings 这类请求转给已有实例，然后自己退出
            if let Ok(hwnd) = FindWindowW(CARD_CLASS, None) {
                let _ = PostMessageW(Some(hwnd), WM_APP + 6, WPARAM(0), LPARAM(0));
            }
            return Ok(());
        }
    }

    register_classes();

    let renderer = Renderer::new()?;
    let scale = win32::primary_scale();
    let dpi = 96.0 * scale;
    let sampler = Sampler::start();
    let mut cfg = Config::load();

    let card_w = ((ui::CARD_W + ui::CARD_MARGIN * 2.0) * scale).round() as i32;
    let card_h = ((ui::CARD_H + ui::CARD_MARGIN * 2.0) * scale).round() as i32;
    let panel_w = (ui::PANEL_W * scale).round() as i32;
    let panel_h = (ui::PANEL_H * scale).round() as i32;
    let settings_w = (ui::SETTINGS_W * scale).round() as i32;
    let settings_h = (ui::SETTINGS_H * scale).round() as i32;

    let wa = win32::work_area_at(0, 0);
    let default_x = wa.right - card_w - 24;
    let default_y = wa.top + (((wa.bottom - wa.top) as f32) * 0.38) as i32;
    let mut x = cfg.x.unwrap_or(default_x).clamp(wa.left, (wa.right - card_w).max(wa.left));
    let y = cfg.y.unwrap_or(default_y).clamp(wa.top, (wa.bottom - card_h).max(wa.top));
    if cfg.dock == Dock::Right {
        x = wa.right - card_w;
    } else if cfg.dock == Dock::Left {
        x = wa.left;
    }
    cfg.x = Some(x);
    cfg.y = Some(y);

    let card_hwnd = create_window(CARD_CLASS, card_w, card_h, x, y)?;
    let card_surface = Surface::new(card_w, card_h);
    let card_target = renderer.target(card_surface.dc, card_w, card_h, dpi)?;

    let panel_hwnd = create_window(PANEL_CLASS, panel_w, panel_h, 0, 0)?;
    let panel_surface = Surface::new(panel_w, panel_h);
    let panel_target = renderer.target(panel_surface.dc, panel_w, panel_h, dpi)?;

    let settings_hwnd = create_window(SETTINGS_CLASS, settings_w, settings_h, 0, 0)?;
    let settings_surface = Surface::new(settings_w, settings_h);
    let settings_target = renderer.target(settings_surface.dc, settings_w, settings_h, dpi)?;

    let tray_icon = tray::install(card_hwnd);

    let app = App {
        card: Win { hwnd: card_hwnd, x, y, w: card_w, h: card_h, surface: card_surface, target: card_target },
        panel: Win { hwnd: panel_hwnd, x: 0, y: 0, w: panel_w, h: panel_h, surface: panel_surface, target: panel_target },
        settings: Win {
            hwnd: settings_hwnd,
            x: 0,
            y: 0,
            w: settings_w,
            h: settings_h,
            surface: settings_surface,
            target: settings_target,
        },
        sampler,
        cfg,
        dpi,
        panel_shown: false,
        panel_alpha: 0.0,
        panel_target: 0.0,
        settings_shown: false,
        mouse_card: false,
        mouse_panel: false,
        tracking_card: false,
        tracking_panel: false,
        dragging: false,
        drag_cursor: (0, 0),
        drag_win: (0, 0),
        slider: None,
        settings_hover: 0,
        expand: 0.0,
        expand_target: 0.0,
        pinned: false,
        click_start: (0, 0),
        tray_icon,
    };

    APP.with(|cell| *cell.borrow_mut() = Some(Box::new(app)));
    with_app(|a| {
        a.draw_card();
        unsafe {
            let _ = ShowWindow(a.card.hwnd, SW_SHOWNOACTIVATE);
            SetTimer(Some(a.card.hwnd), TIMER_REFRESH, 1000, None);
        }
        if std::env::args().any(|arg| arg == "--settings") {
            a.open_settings();
        }
    });

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
