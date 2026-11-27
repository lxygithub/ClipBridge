//! 图片剪贴板（M5）：DIB ⇄ PNG。
//!
//! 为什么不用 WIC / GDI+：两个都是 COM 重依赖，且 GDI+ 要额外初始化；
//! 这里只需要「读 CF_DIB → RGBA → PNG」和「PNG → RGBA → CF_DIB」两个方向，
//! 用 `png` crate 自己做编解码，双端（Rust + Android JNI）走同一套像素约定。

use std::io::Cursor;
use std::path::Path;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, SetStretchBltMode,
    StretchBlt, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE, HDC, SRCCOPY,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows_sys::Win32::System::Ole::CF_DIB;

/// 剪贴板里的图：RGBA（自上而下，每像素 4 字节）。
pub struct Bitmap {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// 读 `CF_DIB`。只认 24/32bpp、未压缩的 DIB——微信/浏览器/截图工具都发这个格式，
/// 8bpp 调色板图和 RLE 压缩图直接放弃（转出来也是花的）。
pub unsafe fn read_clipboard() -> Option<Bitmap> {
    if OpenClipboard(std::ptr::null_mut()) == 0 {
        return None;
    }
    let handle = GetClipboardData(CF_DIB as u32);
    if handle.is_null() {
        CloseClipboard();
        return None;
    }
    let base = GlobalLock(handle) as *const u8;
    if base.is_null() {
        CloseClipboard();
        return None;
    }
    let out = parse_dib(base);
    GlobalUnlock(handle);
    CloseClipboard();
    out
}

/// DIB 头 + 像素 → RGBA。抽出来单独实现，方便以后加 BITFIELDS。
unsafe fn parse_dib(base: *const u8) -> Option<Bitmap> {
    let rd_u32 = |off: usize| std::ptr::read_unaligned(base.add(off) as *const u32);
    let rd_i32 = |off: usize| std::ptr::read_unaligned(base.add(off) as *const i32);
    let head = rd_u32(0) as usize;
    if head < 40 {
        return None;
    }
    let w = rd_i32(4);
    let h_raw = rd_i32(8);
    let bpp = std::ptr::read_unaligned(base.add(14) as *const u16);
    let comp = rd_u32(16);
    if w <= 0 || h_raw == 0 || comp != BI_RGB || (bpp != 24 && bpp != 32) {
        return None;
    }
    let top_down = h_raw < 0;
    let h = h_raw.unsigned_abs() as usize;
    let w = w as usize;
    if w > 8192 || h > 8192 {
        return None;
    }
    let src = base.add(head);
    let stride = if bpp == 32 { w * 4 } else { (w * 3 + 3) / 4 * 4 };
    let mut rgba = vec![0u8; w * h * 4];
    let mut any_alpha = false;
    for y in 0..h {
        let row = if top_down { y } else { h - 1 - y };
        let p = src.add(row * stride);
        for x in 0..w {
            let o = (y * w + x) * 4;
            let s = p.add(x * (bpp as usize) / 8);
            let (b, g, r) = (*s, *s.add(1), *s.add(2));
            let a = if bpp == 32 {
                let a = *s.add(3);
                if a != 0 {
                    any_alpha = true;
                }
                a
            } else {
                255
            };
            rgba[o] = r;
            rgba[o + 1] = g;
            rgba[o + 2] = b;
            rgba[o + 3] = a;
        }
    }
    // 32bpp 且 alpha 全是 0：多半是没带 alpha 通道的截图，按不透明处理
    if bpp == 32 && !any_alpha {
        for px in rgba.chunks_mut(4) {
            px[3] = 255;
        }
    }
    Some(Bitmap {
        rgba,
        w: w as u32,
        h: h as u32,
    })
}

pub fn encode_png(bmp: &Bitmap) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(Cursor::new(&mut out), bmp.w, bmp.h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&bmp.rgba).ok()?;
    }
    Some(out)
}

pub fn decode_png(bytes: &[u8]) -> Option<Bitmap> {
    let mut dec = png::Decoder::new(Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => buf[..info.buffer_size()]
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf[..info.buffer_size()]
            .iter()
            .flat_map(|g| [*g, *g, *g, 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf[..info.buffer_size()]
            .chunks(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => return None,
    };
    if rgba.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    Some(Bitmap {
        rgba,
        w,
        h,
    })
}

/// 把 RGBA 塞进剪贴板（CF_DIB，32bpp，自下而上）。
pub fn write_clipboard(bmp: &Bitmap) -> bool {
    let (w, h) = (bmp.w as usize, bmp.h as usize);
    let head = 40usize;
    let size = head + w * h * 4;
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GHND, size);
        if mem.is_null() {
            CloseClipboard();
            return false;
        }
        let p = GlobalLock(mem) as *mut u8;
        if p.is_null() {
            CloseClipboard();
            return false;
        }
        let wr_u32 = |off: usize, v: u32| std::ptr::write_unaligned(p.add(off) as *mut u32, v);
        let wr_i32 = |off: usize, v: i32| std::ptr::write_unaligned(p.add(off) as *mut i32, v);
        wr_u32(0, 40); // biSize
        wr_i32(4, bmp.w as i32);
        wr_i32(8, bmp.h as i32); // 正高度 = 自下而上
        std::ptr::write_unaligned(p.add(12) as *mut u16, 1); // planes
        std::ptr::write_unaligned(p.add(14) as *mut u16, 32); // bitcount
        wr_u32(16, BI_RGB);
        wr_u32(20, (w * h * 4) as u32);
        for y in 0..h {
            let src_row = h - 1 - y;
            for x in 0..w {
                let s = (src_row * w + x) * 4;
                let d = head + (y * w + x) * 4;
                *p.add(d) = bmp.rgba[s + 2]; // B
                *p.add(d + 1) = bmp.rgba[s + 1]; // G
                *p.add(d + 2) = bmp.rgba[s]; // R
                *p.add(d + 3) = 255;
            }
        }
        GlobalUnlock(mem);
        let ok = SetClipboardData(CF_DIB as u32, mem) != std::ptr::null_mut();
        CloseClipboard();
        ok
    }
}

/// 列表里的小缩略图：一张已经缩放好的 32bpp 位图 + 配套内存 DC。
pub struct Thumb {
    pub hbmp: isize,
    pub dc: isize,
    pub w: i32,
    pub h: i32,
}

// 位图句柄只在 UI 线程用；放进 Mutex 缓存需要能跨线程搬运这个「句柄数字」。
unsafe impl Send for Thumb {}
unsafe impl Sync for Thumb {}

impl Thumb {
    pub unsafe fn destroy(&self) {
        if self.hbmp != 0 {
            DeleteObject(self.hbmp as _);
        }
        if self.dc != 0 {
            DeleteDC(self.dc as HDC);
        }
    }
}

/// 从 PNG 文件生成缩略图（长边 `max` 像素，最近邻采样——缩略图够用，也不必引 `image`）。
pub unsafe fn make_thumb(path: &Path, max: i32) -> Option<Thumb> {
    let bytes = std::fs::read(path).ok()?;
    let src = decode_png(&bytes)?;
    let (sw, sh) = (src.w as i32, src.h as i32);
    let scale = (max as f32 / sw.max(sh) as f32).min(1.0);
    let tw = ((sw as f32 * scale).round() as i32).max(1);
    let th = ((sh as f32 * scale).round() as i32).max(1);

    let dc = CreateCompatibleDC(std::ptr::null_mut());
    if dc.is_null() {
        return None;
    }
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: tw,
        biHeight: -th, // 负高度 = 自上而下，和我们拼像素的顺序一致
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if hbmp.is_null() || bits.is_null() {
        DeleteDC(dc);
        return None;
    }
    let dst = bits as *mut u8;
    for y in 0..th as usize {
        let sy = (y * sh as usize / th as usize).min(sh as usize - 1);
        for x in 0..tw as usize {
            let sx = (x * sw as usize / tw as usize).min(sw as usize - 1);
            let s = (sy * sw as usize + sx) * 4;
            let d = (y * tw as usize + x) * 4;
            *dst.add(d) = src.rgba[s + 2];
            *dst.add(d + 1) = src.rgba[s + 1];
            *dst.add(d + 2) = src.rgba[s];
            *dst.add(d + 3) = 255;
        }
    }
    SelectObject(dc, hbmp as _);
    Some(Thumb {
        hbmp: hbmp as isize,
        dc: dc as isize,
        w: tw,
        h: th,
    })
}

/// 把缩略图贴到 `box_` 里（等比居中，多余留白）。
pub unsafe fn draw_thumb(hdc: HDC, thumb: &Thumb, box_: RECT) {
    let bw = box_.right - box_.left;
    let bh = box_.bottom - box_.top;
    let (dw, dh) = if thumb.w * bh <= thumb.h * bw {
        (thumb.w * bh / thumb.h.max(1), bh)
    } else {
        (bw, thumb.h * bw / thumb.w.max(1))
    };
    let dx = box_.left + (bw - dw) / 2;
    let dy = box_.top + (bh - dh) / 2;
    SetStretchBltMode(hdc, HALFTONE as i32);
    StretchBlt(
        hdc,
        dx,
        dy,
        dw,
        dh,
        thumb.dc as HDC,
        0,
        0,
        thumb.w,
        thumb.h,
        SRCCOPY,
    );
}

/// 只是为了让 `HWND` 的引用不被判成未使用（未来若做拖图入窗要用）。
#[allow(dead_code)]
pub fn _hwnd_marker(_h: HWND) {}
