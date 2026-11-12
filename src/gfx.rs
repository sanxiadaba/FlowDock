//! Direct2D + DirectWrite 渲染封装：画到分层窗口的 DIB 上，坐标一律用 DIP。

use std::io::Write;

use std::cell::RefCell;
use std::collections::HashMap;

use windows::core::Result;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::HDC;

pub fn log(s: &str) {
    let path = std::env::temp_dir().join("flowdock.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{s}");
    }
}

pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: a as f32 / 255.0,
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Right,
    Center,
}

pub struct Renderer {
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            Ok(Renderer { d2d, dwrite })
        }
    }

    pub fn target(&self, hdc: HDC, w: i32, h: i32, dpi: f32) -> Result<Target> {
        unsafe {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = self.d2d.CreateDCRenderTarget(&props)?;
            let bounds = RECT { left: 0, top: 0, right: w, bottom: h };
            rt.BindDC(hdc, &bounds)?;
            Ok(Target {
                rt,
                dwrite: self.dwrite.clone(),
                factory: self.d2d.clone(),
                formats: RefCell::new(HashMap::new()),
                brushes: RefCell::new(HashMap::new()),
            })
        }
    }
}

pub struct Target {
    rt: ID2D1DCRenderTarget,
    dwrite: IDWriteFactory,
    factory: ID2D1Factory,
    /// 文字格式缓存：每帧重复 CreateTextFormat 会做一次字体查找
    formats: RefCell<HashMap<(u32, u32, bool), IDWriteTextFormat>>,
    /// 按颜色缓存画刷：D2D 会延迟到 EndDraw 才真正栅格化，
    /// 复用一支画刷再改色可能让整帧都变成最后的颜色，所以按色缓存。
    brushes: RefCell<HashMap<u32, ID2D1SolidColorBrush>>,
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

impl Target {
    pub fn begin(&self) {
        unsafe {
            let _ = self.rt.BeginDraw();
        }
    }

    pub fn end(&self) {
        unsafe {
            if let Err(e) = self.rt.EndDraw(None, None) {
                log(&format!("EndDraw failed: {e:?}"));
            }
        }
    }

    pub fn clear(&self) {
        unsafe {
            let _ = self.rt.Clear(Some(&rgba(0, 0, 0, 0)));
        }
    }

    fn brush(&self, c: D2D1_COLOR_F) -> Option<ID2D1SolidColorBrush> {
        let key = ((c.r * 255.0) as u32) << 24
            | ((c.g * 255.0) as u32) << 16
            | ((c.b * 255.0) as u32) << 8
            | (c.a * 255.0) as u32;
        if let Some(b) = self.brushes.borrow().get(&key) {
            return Some(b.clone());
        }
        match unsafe { self.rt.CreateSolidColorBrush(&c, None) } {
            Ok(b) => {
                self.brushes.borrow_mut().insert(key, b.clone());
                Some(b)
            }
            Err(_) => None,
        }
    }

    pub fn fill_rect(&self, x: f32, y: f32, w: f32, h: f32, c: D2D1_COLOR_F) {
        unsafe {
            if let Some(b) = self.brush(c) {
                self.rt.FillRectangle(&rect(x, y, w, h), &b);
            }
        }
    }

    pub fn fill_round(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: D2D1_COLOR_F) {
        unsafe {
            if let Some(b) = self.brush(c) {
                let rr = D2D1_ROUNDED_RECT { rect: rect(x, y, w, h), radiusX: r, radiusY: r };
                self.rt.FillRoundedRectangle(&rr, &b);
            }
        }
    }

    pub fn stroke_round(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: D2D1_COLOR_F, width: f32) {
        unsafe {
            if let Some(b) = self.brush(c) {
                let rr = D2D1_ROUNDED_RECT { rect: rect(x, y, w, h), radiusX: r, radiusY: r };
                self.rt.DrawRoundedRectangle(&rr, &b, width, None);
            }
        }
    }

    pub fn draw_icon(&self, icon: &crate::icon::IconData, x: f32, y: f32, size: f32) {
        unsafe {
            let props = D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
            };
            let sz = D2D_SIZE_U { width: icon.w, height: icon.h };
            if let Ok(bmp) = self.rt.CreateBitmap(sz, Some(icon.bgra.as_ptr() as *const core::ffi::c_void), icon.w * 4, &props) {
                let dest = rect(x, y, size, size);
                let src = rect(0.0, 0.0, icon.w as f32, icon.h as f32);
                self.rt.DrawBitmap(&bmp, Some(&dest), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, Some(&src));
            }
        }
    }

    pub fn fill_circle(&self, cx: f32, cy: f32, r: f32, c: D2D1_COLOR_F) {
        unsafe {
            if let Some(b) = self.brush(c) {
                let e = D2D1_ELLIPSE {
                    point: windows_numerics::Vector2 { X: cx, Y: cy },
                    radiusX: r,
                    radiusY: r,
                };
                self.rt.FillEllipse(&e, &b);
            }
        }
    }

    pub fn stroke_circle(&self, cx: f32, cy: f32, r: f32, c: D2D1_COLOR_F, width: f32) {
        unsafe {
            if let Some(b) = self.brush(c) {
                let e = D2D1_ELLIPSE {
                    point: windows_numerics::Vector2 { X: cx, Y: cy },
                    radiusX: r,
                    radiusY: r,
                };
                self.rt.DrawEllipse(&e, &b, width, None);
            }
        }
    }

    fn format_weight(&self, size: f32, align: Align, bold: bool) -> Result<IDWriteTextFormat> {
        let key = ((size * 10.0) as u32, align as u32, bold);
        if let Some(f) = self.formats.borrow().get(&key) {
            return Ok(f.clone());
        }
        unsafe {
            let fmt = self.dwrite.CreateTextFormat(
                windows::core::w!("Microsoft YaHei"),
                None,
                if bold { DWRITE_FONT_WEIGHT_SEMI_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL },
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                windows::core::w!("zh-cn"),
            )?;
            let a = match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
            };
            fmt.SetTextAlignment(a)?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            self.formats.borrow_mut().insert(key, fmt.clone());
            Ok(fmt)
        }
    }

    pub fn text(&self, s: &str, x: f32, y: f32, w: f32, h: f32, size: f32, c: D2D1_COLOR_F, align: Align) {
        self.text_impl(s, x, y, w, h, size, c, align, false);
    }

    pub fn text_bold(&self, s: &str, x: f32, y: f32, w: f32, h: f32, size: f32, c: D2D1_COLOR_F, align: Align) {
        self.text_impl(s, x, y, w, h, size, c, align, true);
    }

    fn text_impl(
        &self,
        s: &str,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        size: f32,
        c: D2D1_COLOR_F,
        align: Align,
        bold: bool,
    ) {
        unsafe {
            let fmt = match self.format_weight(size, align, bold) {
                Ok(f) => f,
                Err(e) => {
                    log(&format!("CreateTextFormat failed: {e:?}"));
                    return;
                }
            };
            let Some(b) = self.brush(c) else {
                log("CreateSolidColorBrush failed");
                return;
            };
            let utf16: Vec<u16> = s.encode_utf16().collect();
            self.rt.DrawText(
                &utf16,
                &fmt,
                &rect(x, y, w, h),
                &b,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    pub fn text_width(&self, s: &str, size: f32) -> f32 {
        unsafe {
            let Ok(fmt) = self.format_weight(size, Align::Left, false) else { return 0.0 };
            let utf16: Vec<u16> = s.encode_utf16().collect();
            let Ok(layout) = self.dwrite.CreateTextLayout(&utf16, &fmt, 4096.0, 128.0) else {
                return 0.0;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_ok() {
                m.widthIncludingTrailingWhitespace
            } else {
                0.0
            }
        }
    }

    pub fn ellipsize(&self, s: &str, size: f32, max_w: f32) -> String {
        if self.text_width(s, size) <= max_w {
            return s.to_string();
        }
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::new();
        for ch in chars {
            let mut probe = out.clone();
            probe.push(ch);
            probe.push('…');
            if self.text_width(&probe, size) > max_w {
                break;
            }
            out.push(ch);
        }
        out.push('…');
        out
    }

    /// 上下箭头：一根短杆 + 一个三角，比字体里的箭头更接近火绒的实心感。
    pub fn arrow(&self, x: f32, y: f32, size: f32, up: bool, c: D2D1_COLOR_F) {
        unsafe {
            let Some(b) = self.brush(c) else { return };
            let cx = x + size / 2.0;
            let shaft_w = (size * 0.16).max(1.0);
            let head_h = size * 0.45;
            let head_w = size * 0.62;

            let (sy, sh, ty) = if up {
                (y + head_h + 1.0, size - head_h - 1.0, y)
            } else {
                (y, size - head_h - 1.0, y + size - head_h)
            };
            self.rt.FillRectangle(&rect(cx - shaft_w / 2.0, sy, shaft_w, sh), &b);

            let Ok(geo) = self.factory.CreatePathGeometry() else { return };
            let Ok(sink) = geo.Open() else { return };
            let p0 = windows_numerics::Vector2 { X: cx - head_w / 2.0, Y: if up { ty + head_h } else { ty } };
            let p1 = windows_numerics::Vector2 { X: cx + head_w / 2.0, Y: if up { ty + head_h } else { ty } };
            let p2 = windows_numerics::Vector2 { X: cx, Y: if up { ty } else { ty + head_h } };
            let _ = sink.BeginFigure(p0, D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLine(p1);
            sink.AddLine(p2);
            let _ = sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            let _ = sink.Close();
            let _ = self.rt.FillGeometry(&geo, &b, None);
        }
    }
}
