//! Win32 底座：分层窗口（per-pixel alpha）+ DIB 表面 + 命中测试。

use std::ffi::c_void;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_TRAY: u32 = WM_APP + 1;

/// 一块 32bpp 预乘 ARGB 的 DIB 表面，配合 UpdateLayeredWindow 实现圆角+阴影。
pub struct Surface {
    pub w: i32,
    pub h: i32,
    pub dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
}

impl Surface {
    pub fn new(w: i32, h: i32) -> Self {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = 0; // BI_RGB

            let mut bits: *mut c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("CreateDIBSection failed");
            let old = SelectObject(dc, dib.into());
            Surface { w, h, dc, dib, old, bits: bits as *mut u32 }
        }
    }

    /// 第 y 行像素的起始指针（BGRA 预乘），截图用。
    pub fn row_ptr(&self, y: i32) -> *const u8 {
        unsafe { (self.bits as *const u8).add((y * self.w * 4) as usize) }
    }

    pub fn present(&self, hwnd: HWND, x: i32, y: i32, alpha: u8) {
        unsafe {
            let pt_dst = POINT { x, y };
            let size = SIZE { cx: self.w, cy: self.h };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: alpha,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                hwnd,
                None,
                Some(&pt_dst),
                Some(&size),
                Some(self.dc),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    /// 顶端像素的 alpha，用于命中测试（阴影区域应当点穿）。
    pub fn alpha_at(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return 0;
        }
        unsafe {
            let px = *self.bits.add((y * self.w + x) as usize);
            ((px >> 24) & 0xFF) as u8
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.dib.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// 主屏 DPI 缩放系数。
pub fn primary_scale() -> f32 {
    unsafe {
        let dc = GetDC(None);
        let dpi = GetDeviceCaps(Some(dc), LOGPIXELSX) as f32;
        ReleaseDC(None, dc);
        (dpi / 96.0).max(1.0)
    }
}

pub struct WorkArea {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

pub fn work_area_at(x: i32, y: i32) -> WorkArea {
    unsafe {
        let pt = POINT { x, y };
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut info);
        WorkArea { left: info.rcWork.left, top: info.rcWork.top, right: info.rcWork.right, bottom: info.rcWork.bottom }
    }
}

pub fn cursor_pos() -> (i32, i32) {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}
