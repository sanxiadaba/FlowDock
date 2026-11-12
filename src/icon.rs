//! 进程图标：SHGetFileInfo 取 32×32 HICON，转成预乘 BGRA 像素缓存起来。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP,
    BITMAPINFO, DIB_RGB_COLORS,
};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

pub struct IconData {
    pub w: u32,
    pub h: u32,
    /// 预乘 BGRA
    pub bgra: Vec<u8>,
}

static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<IconData>>>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, Option<Arc<IconData>>>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn get(path: &str) -> Option<Arc<IconData>> {
    if path.is_empty() {
        return None;
    }
    if let Ok(map) = cache().lock() {
        if let Some(hit) = map.get(path) {
            return hit.clone();
        }
    }
    let icon = extract(path).map(Arc::new);
    if let Ok(mut map) = cache().lock() {
        map.insert(path.to_string(), icon.clone());
    }
    icon
}

fn extract(path: &str) -> Option<IconData> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut info = SHFILEINFOW::default();
        let ok = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            Default::default(),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || info.hIcon.is_invalid() {
            return None;
        }
        let hicon = info.hIcon;

        let mut ii = ICONINFO::default();
        if GetIconInfo(hicon, &mut ii).is_err() {
            let _ = DestroyIcon(hicon);
            return None;
        }

        let mut bm = BITMAP::default();
        let got = GetObjectW(
            ii.hbmColor.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut _ as *mut c_void),
        );
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            cleanup(ii, hicon);
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight);

        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = 0;

        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            dc,
            ii.hbmColor,
            0,
            h as u32,
            Some(bgra.as_mut_ptr() as *mut c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );

        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);

        if lines == 0 {
            cleanup(ii, hicon);
            return None;
        }

        // 有的图标没有 alpha，用 mask 兜底
        let has_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
        if !has_alpha {
            for p in bgra.chunks_exact_mut(4) {
                p[3] = 255;
            }
        }

        // D2D 要预乘
        for p in bgra.chunks_exact_mut(4) {
            let a = p[3] as u32;
            for c in p.iter_mut().take(3) {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }

        cleanup(ii, hicon);
        Some(IconData { w: w as u32, h: h as u32, bgra })
    }
}

unsafe fn cleanup(ii: ICONINFO, hicon: windows::Win32::UI::WindowsAndMessaging::HICON) {
    if !ii.hbmColor.is_invalid() {
        let _ = DeleteObject(ii.hbmColor.into());
    }
    if !ii.hbmMask.is_invalid() {
        let _ = DeleteObject(ii.hbmMask.into());
    }
    let _ = DestroyIcon(hicon);
}
