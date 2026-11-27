//! 图片剪贴板（M5）：DIB ⇄ PNG。
//!
//! 为什么不用 WIC / GDI+：两个都是 COM 重依赖，且 GDI+ 要额外初始化；
//! 这里只需要「读 CF_DIB → RGBA → PNG」和「PNG → RGBA → CF_DIB」两个方向，
//! 用 `png` crate 自己做编解码，双端（Rust + Android JNI）走同一套像素约定。

use std::io::Cursor;
use std::path::Path;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDIBits, GetObjectW,
    SelectObject, SetStretchBltMode, StretchBlt, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_BITFIELDS,
    BI_RGB, DIB_RGB_COLORS, HALFTONE, HDC, SRCCOPY,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW,
    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GHND};
use windows_sys::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_DIBV5};

/// 剪贴板里的图：RGBA（自上而下，每像素 4 字节）。
pub struct Bitmap {
    pub rgba: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// 读剪贴板里的图。按「信息量从高到低」依次试四种格式：
/// 1. 注册格式 `PNG`（浏览器截图、Snipaste 这类会直接给 PNG 字节，无损且不用猜 DIB 头）；
/// 2. `CF_DIB`（绝大多数程序的默认图格式，可能是 8/16/24/32bpp 或 BI_BITFIELDS）；
/// 3. `CF_DIBV5`（带 alpha 掩码的 V5 头，Office/Photoshop 走这条）；
/// 4. `CF_BITMAP`（最后兜底：扔给 GDI 的 GetDIBits 转成 32bpp，什么压缩都能读）。
pub unsafe fn read_clipboard() -> Option<Bitmap> {
    if OpenClipboard(std::ptr::null_mut()) == 0 {
        return None;
    }
    let out = read_png()
        .or_else(|| read_dib(CF_DIB as u32))
        .or_else(|| read_dib(CF_DIBV5 as u32))
        .or_else(|| read_bitmap());
    CloseClipboard();
    out
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 剪贴板里现在有哪些格式（只用于诊断日志：读图失败时打出来，好知道是谁的格式没吃下）。
pub fn describe_formats() -> String {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return "剪贴板被占用".to_string();
        }
        let mut out = Vec::new();
        let mut fmt = EnumClipboardFormats(0);
        while fmt != 0 {
            let mut buf = [0u16; 128];
            let n = GetClipboardFormatNameW(fmt, buf.as_mut_ptr(), buf.len() as i32);
            let name = if n > 0 {
                String::from_utf16_lossy(&buf[..n as usize])
            } else {
                format!("cf#{fmt}")
            };
            out.push(name);
            fmt = EnumClipboardFormats(fmt);
        }
        CloseClipboard();
        out.join(",")
    }
}

unsafe fn read_png() -> Option<Bitmap> {
    let fmt = RegisterClipboardFormatW(wide("PNG").as_ptr());
    if fmt == 0 {
        return None;
    }
    let h = GetClipboardData(fmt as u32);
    if h.is_null() {
        return None;
    }
    let size = GlobalSize(h);
    if size < 8 {
        return None;
    }
    let p = GlobalLock(h) as *const u8;
    if p.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(p, size).to_vec();
    GlobalUnlock(h);
    decode_png(&bytes)
}

unsafe fn read_dib(fmt: u32) -> Option<Bitmap> {
    let handle = GetClipboardData(fmt);
    if handle.is_null() {
        return None;
    }
    let base = GlobalLock(handle) as *const u8;
    if base.is_null() {
        return None;
    }
    let out = parse_dib(base);
    GlobalUnlock(handle);
    out
}

/// 最后兜底：让 GDI 把任何位图（含 RLE、调色板、BITFIELDS）统一转成 32bpp 顶朝下。
unsafe fn read_bitmap() -> Option<Bitmap> {
    let hbmp = GetClipboardData(CF_BITMAP as u32);
    if hbmp.is_null() {
        return None;
    }
    let mut bm: BITMAP = std::mem::zeroed();
    let n = GetObjectW(
        hbmp,
        std::mem::size_of::<BITMAP>() as i32,
        &mut bm as *mut _ as *mut std::ffi::c_void,
    );
    if n == 0 {
        return None;
    }
    let w = bm.bmWidth;
    let h = bm.bmHeight;
    if w <= 0 || h <= 0 || w > 8192 || h > 8192 {
        return None;
    }
    let dc = CreateCompatibleDC(std::ptr::null_mut());
    if dc.is_null() {
        return None;
    }
    let mut info: BITMAPINFO = std::mem::zeroed();
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..std::mem::zeroed()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let got = GetDIBits(
        dc,
        hbmp,
        0,
        h as u32,
        buf.as_mut_ptr() as *mut std::ffi::c_void,
        &mut info,
        DIB_RGB_COLORS,
    );
    DeleteDC(dc);
    if got == 0 {
        return None;
    }
    // GetDIBits 给的是 BGRA，且 GDI 常把 alpha 补 0，统一按不透明处理
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    for i in 0..(w * h) as usize {
        rgba[i * 4] = buf[i * 4 + 2];
        rgba[i * 4 + 1] = buf[i * 4 + 1];
        rgba[i * 4 + 2] = buf[i * 4];
        rgba[i * 4 + 3] = 255;
    }
    Some(Bitmap {
        rgba,
        w: w as u32,
        h: h as u32,
    })
}

/// 按掩码把一个像素通道拉到 0..255（BI_BITFIELDS 里红绿蓝可能不在整字节上）。
fn chan(v: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 255;
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    if bits == 0 {
        return 0;
    }
    let raw = (v & mask) >> shift;
    let max = (1u32 << bits) - 1;
    ((raw * 255 + max / 2) / max) as u8
}

/// DIB 头 + 像素 → RGBA。认 1/4/8bpp 调色板、16/24/32bpp BI_RGB、32/16bpp BI_BITFIELDS，
/// 也认 V5 头（掩码在 40..56 偏移处）。RLE 不理，留给 CF_BITMAP 那条兜底。
unsafe fn parse_dib(base: *const u8) -> Option<Bitmap> {
    let rd_u32 = |off: usize| std::ptr::read_unaligned(base.add(off) as *const u32);
    let rd_i32 = |off: usize| std::ptr::read_unaligned(base.add(off) as *const i32);
    let rd_u16 = |off: usize| std::ptr::read_unaligned(base.add(off) as *const u16);
    let head = rd_u32(0) as usize;
    if head < 40 {
        return None;
    }
    let w = rd_i32(4);
    let h_raw = rd_i32(8);
    let bpp = rd_u16(14) as usize;
    let comp = rd_u32(16);
    let clr_used = rd_u32(32) as usize;
    if w <= 0 || h_raw == 0 || w > 8192 || h_raw.unsigned_abs() > 8192 {
        return None;
    }
    let top_down = h_raw < 0;
    let h = h_raw.unsigned_abs() as usize;
    let w = w as usize;

    // 像素起点：8bpp 及以下跟在调色板后面；BI_BITFIELDS 在 40 字节老头上时，
    // 三张掩码紧跟在头后面（40..52），像素从 52 开始（.NET / Office 常走这条）
    let mut src_off = head;
    if comp == BI_RGB && bpp <= 8 {
        let n = if clr_used > 0 { clr_used } else { 1usize << bpp };
        src_off += n * 4;
    }
    let stride = ((w * bpp + 31) / 32) * 4;
    let mut masks = [0u32, 0, 0, 0];
    if comp == BI_BITFIELDS {
        if head >= 52 {
            // V4/V5 头：掩码就在头里
            masks = [
                rd_u32(40),
                rd_u32(44),
                rd_u32(48),
                if head >= 56 { rd_u32(52) } else { 0 },
            ];
        } else {
            // 40 字节的老头：掩码外挂
            masks = [rd_u32(40), rd_u32(44), rd_u32(48), 0];
            src_off = head + 12;
        }
    }

    let mut rgba = vec![0u8; w * h * 4];
    let mut any_alpha = false;
    for y in 0..h {
        let row = if top_down { y } else { h - 1 - y };
        let p = base.add(src_off + row * stride);
        for x in 0..w {
            let o = (y * w + x) * 4;
            match (comp, bpp) {
                // 24/32bpp 裸 BGRA
                (BI_RGB, 24) | (BI_RGB, 32) => {
                    let s = p.add(x * (bpp / 8));
                    rgba[o] = *s.add(2);
                    rgba[o + 1] = *s.add(1);
                    rgba[o + 2] = *s;
                    let a = if bpp == 32 {
                        let a = *s.add(3);
                        if a != 0 {
                            any_alpha = true;
                        }
                        a
                    } else {
                        255
                    };
                    rgba[o + 3] = a;
                }
                // 调色板（1/4/8bpp）
                (BI_RGB, 1 | 4 | 8) => {
                    let bit = x * bpp;
                    let b = *p.add(bit / 8);
                    let shift = 8 - bpp - (bit % 8);
                    let idx = ((b as usize) >> shift) & ((1 << bpp) - 1);
                    let pal = base.add(head + idx * 4);
                    rgba[o] = *pal.add(2);
                    rgba[o + 1] = *pal.add(1);
                    rgba[o + 2] = *pal;
                    rgba[o + 3] = 255;
                }
                // 掩码位图（16bpp 5-6-5、32bpp BGRA/ARGB 都在这）
                (BI_BITFIELDS, 16) | (BI_BITFIELDS, 32) => {
                    let v = if bpp == 16 {
                        std::ptr::read_unaligned(p.add(x * 2) as *const u16) as u32
                    } else {
                        std::ptr::read_unaligned(p.add(x * 4) as *const u32)
                    };
                    rgba[o] = chan(v, masks[0]);
                    rgba[o + 1] = chan(v, masks[1]);
                    rgba[o + 2] = chan(v, masks[2]);
                    let a = chan(v, masks[3]);
                    if a != 0 && a != 255 {
                        any_alpha = true;
                    }
                    rgba[o + 3] = a;
                }
                _ => return None,
            }
        }
    }
    // 32bpp 且 alpha 全是 0：多半是没带 alpha 通道的截图，按不透明处理
    if !any_alpha {
        for px in rgba.chunks_mut(4) {
            if px[3] == 0 {
                px[3] = 255;
            }
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
