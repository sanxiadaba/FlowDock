//! 托盘图标：运行时画一个双色圆角方块，不依赖外部资源文件。

use std::ffi::c_void;

use windows::core::Result;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::win32::WM_TRAY;

const ICON_SIZE: i32 = 32;

pub fn install(hwnd: HWND) -> HICON {
    let icon = make_icon().unwrap_or_default();
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        set_tip(&mut nid.szTip, "FlowDock · 流量浮窗");
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
    icon
}

pub fn remove(hwnd: HWND) {
    unsafe {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn set_tip(buf: &mut [u16; 128], s: &str) {
    let utf16: Vec<u16> = s.encode_utf16().take(127).collect();
    buf[..utf16.len()].copy_from_slice(&utf16);
}

fn make_icon() -> Result<HICON> {
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));

        let mut cbmi = BITMAPINFO::default();
        cbmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        cbmi.bmiHeader.biWidth = ICON_SIZE;
        cbmi.bmiHeader.biHeight = -ICON_SIZE;
        cbmi.bmiHeader.biPlanes = 1;
        cbmi.bmiHeader.biBitCount = 32;
        cbmi.bmiHeader.biCompression = 0;

        let mut cbits: *mut c_void = std::ptr::null_mut();
        let color = CreateDIBSection(Some(dc), &cbmi, DIB_RGB_COLORS, &mut cbits, None, 0)?;

        // 双色渐变 + 圆角方块
        let px = cbits as *mut u32;
        let half = ICON_SIZE as f32 / 2.0;
        let inset = 2.0f32;
        let radius = 8.0f32;
        for y in 0..ICON_SIZE {
            for x in 0..ICON_SIZE {
                let fx = x as f32 + 0.5;
                let fy = y as f32 + 0.5;
                let dx = (fx - half).abs() - (half - inset - radius);
                let dy = (fy - half).abs() - (half - inset - radius);
                let dist = if dx > 0.0 && dy > 0.0 {
                    (dx * dx + dy * dy).sqrt() - radius
                } else {
                    dx.max(dy) - radius
                };
                let a = (0.5 - dist).clamp(0.0, 1.0);
                let t = ((fx / ICON_SIZE as f32) * 0.35 + (fy / ICON_SIZE as f32) * 0.65).clamp(0.0, 1.0);
                let r = 255.0 * (1.0 - t) + 52.0 * t;
                let g = 159.0 * (1.0 - t) + 199.0 * t;
                let b = 10.0 * (1.0 - t) + 89.0 * t;
                let alpha = (a * 255.0).round() as u32;
                let pixel = (alpha << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
                *px.add((y * ICON_SIZE + x) as usize) = pixel;
            }
        }

        let mut mbmi = BITMAPINFO::default();
        mbmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        mbmi.bmiHeader.biWidth = ICON_SIZE;
        mbmi.bmiHeader.biHeight = -ICON_SIZE;
        mbmi.bmiHeader.biPlanes = 1;
        mbmi.bmiHeader.biBitCount = 1;
        mbmi.bmiHeader.biCompression = 0;
        let mut mbits: *mut c_void = std::ptr::null_mut();
        let mask = CreateDIBSection(Some(dc), &mbmi, DIB_RGB_COLORS, &mut mbits, None, 0)?;

        let info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info)?;

        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        Ok(icon)
    }
}
