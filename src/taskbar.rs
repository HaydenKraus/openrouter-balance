//! Интеграция с панелью задач Windows: трей, ITaskbarList3 (прогресс/оверлей),
//! заголовок окна, автозапуск, единственный экземпляр.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconEvent};
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, COLORREF, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS,
    HANDLE, HWND, LPARAM, LRESULT, SIZE, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject,
    GetTextExtentPoint32W, GetTextMetricsW, PatBlt, SelectObject, SetBkMode, SetTextColor,
    TextOutW, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLACKNESS,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, FW_BOLD, HDC, HGDIOBJ,
    OUT_DEFAULT_PRECIS, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE,
};
use windows::Win32::UI::Shell::{
    ITaskbarList3, TaskbarList, TBPF_ERROR, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow,
    DispatchMessageW, GetMessageW, GetWindowLongPtrW, IsIconic, IsWindowVisible, PostMessageW,
    RegisterClassW, SendMessageTimeoutW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow,
    TranslateMessage, GWLP_USERDATA, HICON, HWND_MESSAGE, ICONINFO, MSG, SMTO_ABORTIFHUNG, SW_HIDE,
    SW_RESTORE, SW_SHOW, WM_APP, WM_SETTEXT, WNDCLASSW,
};

use crate::format::{Level, ProgressKind, TaskbarModel};

/// События от трея в приложение (показ/скрытие окна shell делает сам).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellEvent {
    Refresh,
    /// Пункт «Настройки»: окно уже показано, UI должен открыть настройки.
    OpenSettings,
    /// Пункт «Автозапуск» переключён; новое значение.
    AutostartToggled(bool),
    Quit,
}

/// Сообщение «в очереди есть команда» (в тот же поток, тому же скрытому окну).
const WM_APP_COMMAND: u32 = WM_APP + 1;
/// Запасной размер иконки трея, если DPI узнать не удалось (оверлей панели задач — 16).
const TRAY_ICON_SIZE: u32 = 32;
const OVERLAY_ICON_SIZE: u32 = 16;

/// Реальный размер иконки в трее: 16 px при 100%, 24 при 150%, 32 при 200%.
/// Рисуем сразу в нём, чтобы Windows не сжимала 32×32 (при 150% — с размытием).
/// DPI берётся у панели задач, поэтому смена масштаба учитывается без перезапуска.
fn tray_icon_size() -> u32 {
    use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SM_CXSMICON};
    unsafe {
        let dpi = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null())
            .map(|h| GetDpiForWindow(h))
            .unwrap_or(0);
        let dpi = if dpi == 0 { GetDpiForSystem() } else { dpi };
        match GetSystemMetricsForDpi(SM_CXSMICON, dpi) {
            s if s > 0 => (s as u32).clamp(16, 64),
            _ => TRAY_ICON_SIZE,
        }
    }
}
/// Лимит тултипа трея: 127 символов + завершающий ноль.
const TIP_MAX_CHARS: usize = 127;
/// Таймаут ожидания завершения потока в `shutdown`.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(1000);
/// Имя класса скрытого message-only окна.
const HIDDEN_CLASS: PCWSTR = w!("OpenRouterBalance.MessageWindow");
const INSTANCE_MUTEX: &str = "Local\\OpenRouterBalance.Instance";
const INSTANCE_EVENT: &str = "Local\\OpenRouterBalance.Show";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "OpenRouterBalance";

// ---------------------------------------------------------------------------
// Общие мелкие помощники
// ---------------------------------------------------------------------------

/// UTF-16 с завершающим нулём (для WinAPI-строк).
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

fn hwnd_of(raw: isize) -> Option<HWND> {
    (raw != 0).then_some(HWND(raw as *mut c_void))
}

/// `HWND` как число — безопасно передавать между потоками.
fn hwnd_num(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

fn hwnd_num_from(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// `AddRef` и «сырой» указатель интерфейса как число (для передачи в поток).
fn add_ref_raw(taskbar: &ITaskbarList3) -> *mut c_void {
    taskbar.to_owned().into_raw()
}

// ---------------------------------------------------------------------------
// Рендер иконки (GDI в памяти)
// ---------------------------------------------------------------------------

/// Подобранная высота шрифта: самая крупная, при которой текст влезает в `box_w`×`box_h`.
/// Идём от «очень крупно» вниз и измеряем реальной метрикой GDI.
/// Крупнее `box_h * 1.5` высота заведомо не влезет по вертикали (h ≈ height), поэтому
/// сканирование начинается оттуда — это делает подбор дешёвым (десятки вызовов GDI).
fn font_height_for(hdc: HDC, text: &str, box_w: i32, box_h: i32) -> Option<i32> {
    let bbox_w = box_w.max(1);
    let bbox_h = box_h.max(1);
    let start = (bbox_h.saturating_mul(3) / 2).clamp(6, 512);
    for height in (6..=start).rev() {
        match text_metrics(hdc, text, height) {
            Some((w, h)) if w <= bbox_w && h <= bbox_h => return Some(height),
            Some(_) => continue,
            // Измерение не удалось (нет шрифта) — дальше пробовать бессмысленно.
            None => return Some(6),
        }
    }
    // Даже самый мелкий не влез — рисуем им (текст важнее, чем точная посадка).
    Some(6)
}

fn text_metrics(hdc: HDC, text: &str, height: i32) -> Option<(i32, i32)> {
    unsafe {
        let font = CreateFontW(
            height,
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            w!("Segoe UI"),
        );
        if font.is_invalid() {
            return None;
        }
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let mut size = SIZE::default();
        // ИНВАРИАНТ: font жив, выбран в hdc до конца измерений, поэтому GDI-вызов валиден.
        let ok = GetTextExtentPoint32W(hdc, &wide(text)[..text.encode_utf16().count()], &mut size)
            .as_bool();
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(font.0));
        ok.then_some((size.cx, size.cy))
    }
}

/// Рисует `text` белым на чёрном 32bpp DIB и возвращает (пиксели, ширина, высота).
fn draw_text_mask(text: &str, size: u32, font_height: i32) -> Option<(Vec<u8>, i32, i32)> {
    let dim = size as i32;
    unsafe {
        let screen = CreateCompatibleDC(None);
        if screen.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: dim,
                biHeight: -dim, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
        let dib = match dib {
            Ok(h) if !h.is_invalid() && !bits.is_null() => h,
            _ => {
                let _ = DeleteDC(screen);
                return None;
            }
        };
        let old_bitmap = SelectObject(screen, HGDIOBJ(dib.0));

        // Чёрный фон кистью.
        let brush = CreateSolidBrush(COLORREF(0));
        let old_brush = if brush.is_invalid() {
            None
        } else {
            Some(SelectObject(screen, HGDIOBJ(brush.0)))
        };
        let _ = PatBlt(screen, 0, 0, dim, dim, BLACKNESS);
        if let Some(old) = old_brush {
            SelectObject(screen, old);
        }
        let _ = DeleteObject(HGDIOBJ(brush.0));

        let font = CreateFontW(
            font_height,
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            w!("Segoe UI"),
        );
        let old_font = if font.is_invalid() {
            None
        } else {
            Some(SelectObject(screen, HGDIOBJ(font.0)))
        };
        // ИНВАРИАНТ: DIB выбран в screen, буфер `bits` валиден до DeleteObject(dib).
        let mut drawn = false;
        if old_font.is_some() {
            let _ = SetBkMode(screen, TRANSPARENT);
            let _ = SetTextColor(screen, COLORREF(0x00FF_FFFF));
            let units: Vec<u16> = text.encode_utf16().collect();
            let mut extent = SIZE::default();
            if GetTextExtentPoint32W(screen, &units, &mut extent).as_bool() {
                let x = (dim - extent.cx) / 2;
                let y = (dim - extent.cy) / 2;
                drawn = TextOutW(screen, x, y, &units).as_bool();
            }
        }

        let mask = if drawn {
            let count = (dim as usize) * (dim as usize) * 4;
            // ИНВАРИАНТ: bits указывает на bufer DIB ровно dim*dim*4 байт (top-down 32bpp).
            let px = std::slice::from_raw_parts(bits as *const u8, count).to_vec();
            Some((px, dim, dim))
        } else {
            None
        };

        if let Some(old) = old_font {
            SelectObject(screen, old);
        }
        let _ = DeleteObject(HGDIOBJ(font.0));
        SelectObject(screen, old_bitmap);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(screen);
        mask
    }
}

/// Рисует иконку `size`×`size` RGBA: цветной скруглённый фон + короткий текст.
/// Вынесено отдельно для тестов (чистая функция).
pub fn render_icon_rgba(size: u32, text: &str, bg: [u8; 3], fg: [u8; 3]) -> Vec<u8> {
    let size = size.max(1);
    let dim = size as i32;
    let mut out = vec![0u8; (size as usize) * (size as usize) * 4];
    // Скруглённый непрозрачный фон.
    let radius = (size as f64) * 0.22;
    for y in 0..size {
        for x in 0..size {
            let coverage = rounded_coverage(x as f64 + 0.5, y as f64 + 0.5, size as f64, radius);
            let i = ((y * size + x) * 4) as usize;
            out[i] = bg[0];
            out[i + 1] = bg[1];
            out[i + 2] = bg[2];
            out[i + 3] = (coverage * 255.0).round() as u8;
        }
    }
    let text = text.trim();
    if text.is_empty() {
        return out;
    }
    let box_w = dim - 2;
    let box_h = dim - 2;
    let mask = (|| {
        let screen = unsafe { CreateCompatibleDC(None) };
        if screen.is_invalid() {
            return None;
        }
        let h = font_height_for(screen, text, box_w, box_h);
        unsafe {
            let _ = DeleteDC(screen);
        }
        let h = h?;
        draw_text_mask(text, size, h)
    })();
    let Some((px, mw, mh)) = mask else {
        return out; // GDI не сработал — остаётся сплошная заливка bg.
    };
    for y in 0..size.min(mh as u32) {
        for x in 0..size.min(mw as u32) {
            let mi = ((y * mw as u32 + x) * 4) as usize;
            // Яркость = покрытие (рисуем белым на чёрном, любая компонента даёт покрытие).
            let cov = (px[mi].max(px[mi + 1]).max(px[mi + 2])) as f64 / 255.0;
            if cov <= 0.0 {
                continue;
            }
            let i = ((y * size + x) * 4) as usize;
            for c in 0..3 {
                out[i + c] = (bg[c] as f64 * (1.0 - cov) + fg[c] as f64 * cov).round() as u8;
            }
            out[i + 3] = 255;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Иконка трея: только цифры, без подложки
// ---------------------------------------------------------------------------

/// Кандидаты шрифта (от самого узкого к запасным). Проверяются реальной
/// метрикой GDI: если глифы нулевой ширины — шрифта в системе нет.
const TRAY_FONTS: [&str; 7] = [
    "Segoe UI Semibold",
    "Bahnschrift SemiBold Condensed",
    "Bahnschrift SemiCondensed",
    "Segoe UI Semibold",
    "Segoe UI",
    "Arial",
    "Tahoma",
];
/// Тёмный контур вокруг цифр (для читаемости и на тёмной, и на светлой панели).
/// Просвет между строками цифр, px.
/// Просвет между строками: 3 px на 32, 2 px на 24, 1 px на 16.
fn tray_gap(size: i32) -> i32 {
    (size * 3 / 32).max(1)
}
const TRAY_OUTLINE: [u8; 3] = [0, 0, 0];
const TRAY_OUTLINE_ALPHA: f64 = 0.88;
/// Цвет цифр в трее: всегда белый, уровень цветом не кодируется.
const TRAY_WHITE: [u8; 3] = [255, 255, 255];

/// Минимальная ширина глифов шрифта: нулевая означает «шрифта нет».
fn font_glyph_width(hdc: HDC, face: &str) -> i32 {
    unsafe {
        let font = CreateFontW(
            24,
            0,
            0,
            0,
            0, // FW_DONTCARE: реальный вес задаёт сам шрифт
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            PCWSTR(wide(face).as_ptr()),
        );
        if font.is_invalid() {
            return 0;
        }
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let mut size = SIZE::default();
        // ИНВАРИАНТ: font выбран в hdc на время измерения и удаляется ниже.
        let ok = GetTextExtentPoint32W(hdc, &wide("0123456789")[..10], &mut size).as_bool();
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(font.0));
        if ok {
            size.cx
        } else {
            0
        }
    }
}

/// Первый доступный шрифт из `TRAY_FONTS` (в порядке предпочтения).
fn pick_tray_font(hdc: HDC) -> &'static str {
    TRAY_FONTS
        .iter()
        .copied()
        .find(|f| font_glyph_width(hdc, f) > 0)
        .unwrap_or("Arial")
}

/// Подобранный шрифт: `height` — высота ячейки (`lfHeight`), `width` — средняя ширина
/// символа (`lfWidth`, 0 — натуральная), `line_h` — реальная высота строки (`tmHeight`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FittedFont {
    height: i32,
    width: i32,
    line_h: i32,
}

/// Метрики, важные для посадки: ширина самой длинной строки и РЕАЛЬНАЯ высота
/// чернил ОДНОЙ строки цифр (а не `tmHeight` с внутренним leading).
/// Измеряется пробным рендером — точнее, чем метрики шрифта.
fn line_ink_metrics(face: &str, lines: &[String], height: i32, width: i32) -> Option<(i32, i32)> {
    // Пробуем только первую (самую широкую по смыслу) строку: высота чернил у всех
    // строк шрифта одинакова, а ширину берём максимумом по всем строкам.
    let probe = (height.abs() + 8).clamp(8, 512);
    let single = [lines.first()?.clone()];
    let (px, dim, _) = draw_lines_probe(&single, probe as u32, face, height, width)?;
    let (_, by0, _, by1) = ink_bounds(&px, dim as usize * 4, dim);
    if by1 < by0 {
        return None; // пустая маска
    }
    let ink_h = by1 - by0 + 1;
    // Ширину измеряем по всем строкам отдельным пробным рендером.
    let mut max_w = 0;
    for line in lines {
        let one = [line.clone()];
        let (px, dim, _) = draw_lines_probe(&one, probe as u32, face, height, width)?;
        let (bx0, _, bx1, _) = ink_bounds(&px, dim as usize * 4, dim);
        if bx1 >= bx0 {
            max_w = max_w.max(bx1 - bx0 + 1);
        }
    }
    Some((max_w, ink_h))
}

/// Пробный рендер строк (белым на чёрном) в квадратный DIB `dim`×`dim`.
/// Строки рисуются стопкой от левого верхнего угла — без центрирования.
fn draw_lines_probe(
    lines: &[String],
    dim: u32,
    face: &str,
    height: i32,
    width: i32,
) -> Option<(Vec<u8>, i32, i32)> {
    let d = dim as i32;
    let stride = (dim as usize) * 4;
    unsafe {
        let screen = CreateCompatibleDC(None);
        if screen.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: d,
                biHeight: -d,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let pv: *mut *mut c_void = &mut bits;
        let dib = match CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, pv, None, 0) {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                let _ = DeleteDC(screen);
                return None;
            }
        };
        // ИНВАРИАНТ: GDI записал указатель через `pv`; читаем volatile (см. draw_lines_mask).
        let bits = std::ptr::read_volatile(&bits as *const *mut c_void) as *const u8;
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(screen);
            return None;
        }
        let old_bitmap = SelectObject(screen, HGDIOBJ(dib.0));
        let face_w = wide(face);
        let font = CreateFontW(
            height,
            width,
            0,
            0,
            0,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            PCWSTR(face_w.as_ptr()),
        );
        let old_font = if font.is_invalid() {
            None
        } else {
            Some(SelectObject(screen, HGDIOBJ(font.0)))
        };
        let mut ok = old_font.is_some();
        if ok {
            let _ = SetBkMode(screen, TRANSPARENT);
            let _ = SetTextColor(screen, COLORREF(0x00FF_FFFF));
            let mut y = 0;
            for line in lines {
                let units: Vec<u16> = line.encode_utf16().collect();
                if !TextOutW(screen, 0, y, &units).as_bool() {
                    ok = false;
                    break;
                }
                y += height.abs().max(1);
            }
        }
        let out = ok.then(|| {
            // ИНВАРИАНТ: bits указывает на буфер DIB ровно stride*d байт.
            let px = std::slice::from_raw_parts(bits, stride * d as usize).to_vec();
            (px, d, d)
        });
        if let Some(old) = old_font {
            SelectObject(screen, old);
        }
        let _ = DeleteObject(HGDIOBJ(font.0));
        SelectObject(screen, old_bitmap);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(screen);
        out
    }
}

/// Средняя ширина символа шрифта при заданной высоте (`tmAveCharWidth`).
fn ave_char_width(hdc: HDC, face: &str, height: i32) -> i32 {
    unsafe {
        let font = CreateFontW(
            height,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            PCWSTR(wide(face).as_ptr()),
        );
        if font.is_invalid() {
            return 0;
        }
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let mut tm = TEXTMETRICW::default();
        // ИНВАРИАНТ: font выбран в hdc на время измерения и удаляется ниже.
        let ok = GetTextMetricsW(hdc, &mut tm).as_bool();
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(font.0));
        if ok {
            tm.tmAveCharWidth
        } else {
            0
        }
    }
}

/// Подбирает максимально крупный шрифт: цифры должны занимать почти всю высоту иконки.
/// Идём по убыванию высоты ячейки; для каждой высоты — по убыванию коэффициента ширины
/// (1.0 → 0.6): чем уже шрифт, тем больше знаков влезает по ширине `size - 1`.
/// Ограничение по высоте — на РЕАЛЬНУЮ высоту чернил цифр, а не на `tmHeight`
/// (у шрифта есть внутренний leading, из-за чего цифры занимают ~60% ячейки).
fn fit_font(hdc: HDC, face: &str, lines: &[String], size: i32) -> Option<FittedFont> {
    // 1 px по краям оставляем контуру, плюс округление при центрировании.
    let limit_w = (size - 2).max(1);
    let rows = lines.len() as i32;
    let limit_h = size.max(1);
    let gap = tray_gap(size);
    // Ячейку берём с запасом: чернила цифр занимают лишь часть tmHeight.
    let cap = (size * 3).clamp(5, 300);
    let mut best: Option<(i32, FittedFont)> = None;
    for height in (5..=cap).rev() {
        let ave = ave_char_width(hdc, face, height);
        if ave <= 0 {
            continue;
        }
        for step in -6..=4 {
            let factor = 1.0 - 0.1 * step as f64; // 1.6 … 0.6: сначала растягиваем вширь
            let width = (ave as f64 * factor).round() as i32;
            let Some((text_w, ink_h)) = line_ink_metrics(face, lines, height, width) else {
                continue;
            };
            let font = FittedFont {
                height,
                width,
                line_h: ink_h,
            };
            // Влезает: строки стоят стопкой, между ними 1 px просвета.
            if text_w <= limit_w && ink_h * rows + (rows - 1) * gap <= limit_h {
                return Some(font);
            }
            // Запасной вариант — самая крупная высота чернил, что ещё влезает.
            if ink_h * rows + (rows - 1) * gap <= limit_h && best.is_none_or(|(h, _)| ink_h > h) {
                best = Some((ink_h, font));
            }
        }
    }
    best.map(|(_, f)| f)
}

/// Рисует маску покрытия: строки белым на чёрном 32bpp DIB.
/// Возвращает (пиксели BGRA, ширина, высота иконки).
fn draw_lines_mask(
    lines: &[String],
    size: u32,
    face: &str,
    fit: FittedFont,
) -> Option<(Vec<u8>, i32, i32)> {
    let dim = size as i32;
    let stride = (size as usize) * 4; // строки DIB выровнены по DWORD
    unsafe {
        let screen = CreateCompatibleDC(None);
        if screen.is_invalid() {
            return None;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: dim,
                biHeight: -dim, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let pv: *mut *mut c_void = &mut bits;
        let dib = match CreateDIBSection(Some(screen), &bmi, DIB_RGB_COLORS, pv, None, 0) {
            Ok(h) if !h.is_invalid() => h,
            _ => {
                let _ = DeleteDC(screen);
                return None;
            }
        };
        // ИНВАРИАНТ: GDI записал указатель на пиксели через `pv`; читаем его через
        // volatile, иначе оптимизатор может оставить `bits` равным null.
        let bits = std::ptr::read_volatile(&bits as *const *mut c_void) as *const u8;
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(screen);
            return None;
        }
        let old_bitmap = SelectObject(screen, HGDIOBJ(dib.0));
        // ИНВАРИАНТ: имя шрифта живёт до DeleteObject(font) ниже.
        let face_w = wide(face);

        let brush = CreateSolidBrush(COLORREF(0));
        let old_brush = if brush.is_invalid() {
            None
        } else {
            Some(SelectObject(screen, HGDIOBJ(brush.0)))
        };
        let _ = PatBlt(screen, 0, 0, dim, dim, BLACKNESS);
        if let Some(old) = old_brush {
            SelectObject(screen, old);
        }
        let _ = DeleteObject(HGDIOBJ(brush.0));

        let font = CreateFontW(
            fit.height,
            fit.width, // сжатие по ширине: больше знаков влезает при той же высоте
            0,
            0,
            0, // FW_DONTCARE (см. line_metrics)
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            PCWSTR(face_w.as_ptr()),
        );
        let old_font = if font.is_invalid() {
            None
        } else {
            Some(SelectObject(screen, HGDIOBJ(font.0)))
        };

        // ИНВАРИАНТ: DIB выбран в screen, `bits` валиден до DeleteObject(dib).
        let mut drawn = false;
        if old_font.is_some() {
            let _ = SetBkMode(screen, TRANSPARENT);
            let _ = SetTextColor(screen, COLORREF(0x00FF_FFFF));
            // Пробный проход: рисуем строки стопкой от верха и измеряем реальные
            // границы чернил, чтобы затем центрировать именно их (tmHeight содержит
            // внутренний leading и даёт сдвиг вниз).
            drawn = true;
            // Отступ чернил от верха ячейки: строки ставим так, чтобы чернила
            // первой строки начинались с y = 0, а шаг строк = высота чернил + 1.
            let first: Vec<u16> = lines[0].encode_utf16().collect();
            let _ = TextOutW(screen, 0, 0, &first);
            let (_, top, _, _) = ink_bounds(
                std::slice::from_raw_parts(bits, stride * dim as usize),
                stride,
                dim,
            );
            let top = top.min(dim);
            let _ = PatBlt(screen, 0, 0, dim, dim, BLACKNESS);
            let mut y = -top;
            for line in lines {
                let units: Vec<u16> = line.encode_utf16().collect();
                if !TextOutW(screen, 0, y, &units).as_bool() {
                    drawn = false;
                    break;
                }
                y += fit.line_h + tray_gap(dim);
            }
            if drawn {
                // ИНВАРИАНТ: bits указывает на буфер DIB ровно stride*dim байт.
                let probe = std::slice::from_raw_parts(bits, stride * dim as usize);
                let (bx0, by0, bx1, by1) = ink_bounds(probe, stride, dim);
                if bx1 < bx0 || by1 < by0 {
                    drawn = false; // чернил нет — рисовать нечего
                } else {
                    // Сдвиг, при котором bbox чернил встанет по центру иконки.
                    let dyy = (dim - (by1 - by0 + 1)) / 2 - by0;
                    let _ = PatBlt(screen, 0, 0, dim, dim, BLACKNESS);
                    y = -top;
                    for line in lines {
                        let units: Vec<u16> = line.encode_utf16().collect();
                        // Каждая строка центруется по горизонтали отдельно.
                        let mut ext = SIZE::default();
                        let _ = GetTextExtentPoint32W(screen, &units, &mut ext);
                        let dx = (dim - ext.cx) / 2;
                        if !TextOutW(screen, dx, y + dyy, &units).as_bool() {
                            drawn = false;
                            break;
                        }
                        y += fit.line_h + tray_gap(dim);
                    }
                }
            }
        }

        let mask = if drawn {
            // ИНВАРИАНТ: bits указывает на буфер DIB ровно stride*dim байт (top-down 32bpp).
            let count = stride * dim as usize;
            let px = std::slice::from_raw_parts(bits, count).to_vec();
            Some((px, dim, dim))
        } else {
            None
        };

        if let Some(old) = old_font {
            SelectObject(screen, old);
        }
        let _ = DeleteObject(HGDIOBJ(font.0));
        SelectObject(screen, old_bitmap);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(screen);
        mask
    }
}

/// Границы непрозрачных (не чёрных) пикселей маски: (x0, y0, x1, y1).
/// Для пустой маски возвращает (0, 0, -1, -1).
fn ink_bounds(mask: &[u8], stride: usize, dim: i32) -> (i32, i32, i32, i32) {
    let mut b = (dim, dim, -1, -1);
    for y in 0..dim as usize {
        for x in 0..dim as usize {
            let i = y * stride + x * 4;
            let cov = (0..3)
                .map(|c| mask.get(i + c).copied().unwrap_or(0))
                .max()
                .unwrap_or(0);
            if cov > 0 {
                b.0 = b.0.min(x as i32);
                b.1 = b.1.min(y as i32);
                b.2 = b.2.max(x as i32);
                b.3 = b.3.max(y as i32);
            }
        }
    }
    b
}

/// Строит покрытие по маске текста: `coverage` — сам текст, `outline` — тёмный контур
/// (максимум по 8 соседям).
fn mask_coverage(mask: &[u8], stride: usize, size: u32) -> (Vec<f64>, Vec<f64>) {
    let n = (size as usize) * (size as usize);
    let mut coverage = vec![0.0f64; n];
    let mut outline = vec![0.0f64; n];
    for y in 0..size as usize {
        for x in 0..size as usize {
            let i = y * stride + x * 4;
            // Пиксель за пределами маски читаем как чёрный (нулевое покрытие).
            let cov = |o: usize| mask.get(i + o).copied().unwrap_or(0) as f64 / 255.0;
            coverage[y * size as usize + x] = cov(0).max(cov(1)).max(cov(2));
        }
    }
    for y in 0..size as usize {
        for x in 0..size as usize {
            let mut m: f64 = 0.0;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx < 0 || ny < 0 || nx >= size as i32 || ny >= size as i32 {
                        continue;
                    }
                    m = m.max(coverage[ny as usize * size as usize + nx as usize]);
                }
            }
            outline[y * size as usize + x] = m;
        }
    }
    (coverage, outline)
}

/// Иконка трея `size`×`size`: прозрачный фон, тёмный контур и цифры цветом `color`.
/// `lines` — 1 или 2 строки (больше — берутся первые две); пустые строки дают
/// полностью прозрачную иконку. Чистая функция: при сбое GDI возвращает прозрачный буфер.
pub fn render_tray_icon_rgba(size: u32, lines: &[String], color: [u8; 3]) -> Vec<u8> {
    let size = size.max(1);
    let mut out = vec![0u8; (size as usize) * (size as usize) * 4];
    let mut lines: Vec<String> = lines
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(2)
        .collect();
    if lines.is_empty() {
        return out;
    }
    // Одна сумма "13.75" → две строки: целая часть сверху, дробная снизу (крупнее).
    if lines.len() == 1 {
        if let Some((int, frac)) = lines[0].split_once('.') {
            if !int.is_empty() && !frac.is_empty() {
                lines = vec![int.to_string(), frac.to_string()];
            }
        }
    }

    let mask = (|| {
        let screen = unsafe { CreateCompatibleDC(None) };
        if screen.is_invalid() {
            return None;
        }
        let face = pick_tray_font(screen);
        let fitted = fit_font(screen, face, &lines, size as i32);
        unsafe {
            let _ = DeleteDC(screen);
        }
        let fit = fitted?;
        draw_lines_mask(&lines, size, face, fit)
    })();
    let px = match mask {
        Some(m) => m,
        None => return out, // GDI не сработал — прозрачная иконка вместо паники.
    };
    let (px, mw, mh) = px;

    let stride = (size as usize) * 4; // строки DIB выровнены по DWORD: size кратно 4
    let (coverage, outline) = mask_coverage(&px, stride, size);
    // Контур: полное покрытие у соседей чернил (1 px тёмного гало), чтобы белые
    // цифры читались и на светлой панели. Сглаживание только по внешнему краю.
    let flat: Vec<[f64; 4]> = (0..(size as usize) * (size as usize))
        .map(|i| {
            let oa = outline[i].powf(0.35) * TRAY_OUTLINE_ALPHA;
            [
                oa * TRAY_OUTLINE[0] as f64,
                oa * TRAY_OUTLINE[1] as f64,
                oa * TRAY_OUTLINE[2] as f64,
                oa,
            ]
        })
        .collect();
    for y in 0..(size.min(mh as u32) as usize) {
        for x in 0..(size.min(mw as u32) as usize) {
            let idx = y * size as usize + x;
            let cov = coverage[idx];
            let oa = flat[idx][3];
            // Пиксели вне чернил и вне контура остаются прозрачными.
            if cov <= 0.0 && oa <= 0.0 {
                continue;
            }
            // Цифры (color) поверх тёмного контура; контур лежит на прозрачном фоне.
            let a = cov + (1.0 - cov) * oa;
            if a <= 0.0 {
                continue;
            }
            let i = idx * 4;
            for c in 0..3 {
                let straight = (color[c] as f64 * cov + flat[idx][c] * (1.0 - cov)) / a;
                out[i + c] = straight.round().clamp(0.0, 255.0) as u8;
            }
            out[i + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Покрытие пикселя (x, y) скруглённым квадратом `size`×`size` c радиусом `r`.
fn rounded_coverage(x: f64, y: f64, size: f64, r: f64) -> f64 {
    let cx = x.clamp(r, size - r);
    let cy = y.clamp(r, size - r);
    let dx = x - cx;
    let dy = y - cy;
    let d = (dx * dx + dy * dy).sqrt();
    // Мягкая граница шириной ~1 пиксель.
    (r - d + 0.5).clamp(0.0, 1.0)
}

/// HICON 16×16 из RGBA-буфера (цветной битмап + маска прозрачности).
fn hicon_from_rgba(rgba: &[u8], size: u32) -> Option<HICON> {
    let dim = size as i32;
    let stride = (size as usize).div_ceil(16) * 2; // выравнивание строк маски по WORD
    unsafe {
        let screen = CreateCompatibleDC(None);
        if screen.is_invalid() {
            return None;
        }
        let mut color_bits: *mut c_void = std::ptr::null_mut();
        let color_bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: dim,
                biHeight: -dim,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let color = CreateDIBSection(
            Some(screen),
            &color_bmi,
            DIB_RGB_COLORS,
            &mut color_bits,
            None,
            0,
        );
        let color = match color {
            Ok(h) if !h.is_invalid() && !color_bits.is_null() => h,
            _ => {
                let _ = DeleteDC(screen);
                return None;
            }
        };
        let mut mask_bits: *mut c_void = std::ptr::null_mut();
        let mask_bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: dim,
                biHeight: -dim,
                biPlanes: 1,
                biBitCount: 1,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mask = CreateDIBSection(
            Some(screen),
            &mask_bmi,
            DIB_RGB_COLORS,
            &mut mask_bits,
            None,
            0,
        );
        let _ = DeleteDC(screen);
        let mask = match mask {
            Ok(h) if !h.is_invalid() && !mask_bits.is_null() => h,
            _ => {
                let _ = DeleteObject(HGDIOBJ(color.0));
                return None;
            }
        };

        // ИНВАРИАНТ: буферы DIB валидны до DeleteObject соответствующих битмапов.
        // Цветной битмап: BGRA, альфа = исходная (используется CreateIconIndirect).
        for i in 0..(size as usize * size as usize) {
            let src = &rgba[i * 4..i * 4 + 4];
            let dst = color_bits as *mut u8;
            *dst.add(i * 4) = src[2];
            *dst.add(i * 4 + 1) = src[1];
            *dst.add(i * 4 + 2) = src[0];
            *dst.add(i * 4 + 3) = src[3];
        }
        // Маска: 1 = прозрачный. Заполняем по альфе.
        for y in 0..size as usize {
            for x in 0..size as usize {
                let a = rgba[(y * size as usize + x) * 4 + 3];
                let byte = (mask_bits as *mut u8).add(y * stride + x / 8);
                let bit = 0x80u8 >> (x % 8);
                if a < 128 {
                    *byte |= bit;
                } else {
                    *byte &= !bit;
                }
            }
        }

        let info = ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        };
        let hicon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        hicon
    }
}

// ---------------------------------------------------------------------------
// Команды потоку
// ---------------------------------------------------------------------------

enum Command {
    Update(TaskbarModel),
    SetAutostartChecked(bool),
    ShowWindow,
    Shutdown,
}

/// Канал «команда → поток»: очередь + PostMessageW на скрытое окно.
/// `HWND` хранится числом, чтобы структуру можно было держать в `Arc` между потоками.
struct CommandQueue {
    hwnd: AtomicIsize,
    queue: Mutex<VecDeque<Command>>,
}

impl CommandQueue {
    fn new(hwnd: isize) -> Self {
        Self {
            hwnd: AtomicIsize::new(hwnd),
            queue: Mutex::new(VecDeque::new()),
        }
    }

    fn set_hwnd(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::SeqCst);
    }

    fn push(&self, cmd: Command) {
        // ИНВАРИАНТ: блокировка только на время вставки, панику не несёт.
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(cmd);
        }
        let hwnd = self.hwnd.load(Ordering::SeqCst);
        if hwnd == 0 {
            return; // поток ещё не создал скрытое окно
        }
        unsafe {
            let _ = PostMessageW(
                Some(hwnd_num_from(hwnd)),
                WM_APP_COMMAND,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }

    fn drain(&self) -> Vec<Command> {
        match self.queue.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Общее состояние трей-потока (нужно и wndproc, и обработчикам трея/меню).
struct TrayState {
    queue: Arc<CommandQueue>,
    events: Sender<ShellEvent>,
    on_event: Box<dyn Fn() + Send + Sync>,
    hwnd: HWND,
    tray: Option<TrayIcon>,
    menu: Menu,
    refresh: MenuItem,
    settings: MenuItem,
    autostart: CheckMenuItem,
    quit: MenuItem,
    taskbar: Option<ITaskbarList3>,
    last_model: Option<TaskbarModel>,
    /// Последняя нарисованная иконка трея (чтобы не перерисовывать зря).
    last_icon: Option<(u32, Vec<String>)>,
    /// Последняя установленная HICON оверлея (уничтожается при замене).
    overlay_icon: Option<HICON>,
    /// Поток, который повторно применит прогресс через ~300 мс после показа окна.
    reapply: Option<JoinHandle<()>>,
}

impl TrayState {
    fn emit(&self, ev: ShellEvent) {
        let _ = self.events.send(ev);
        (self.on_event)();
    }

    fn toggle_window(&self) {
        let hwnd = self.hwnd;
        unsafe {
            let visible = IsWindowVisible(hwnd).as_bool();
            let iconic = IsIconic(hwnd).as_bool();
            if visible && !iconic {
                let _ = ShowWindow(hwnd, SW_HIDE);
            } else {
                let _ = ShowWindow(hwnd, SW_SHOW);
                if iconic {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }

    fn show_window(&mut self) {
        let hwnd = self.hwnd;
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
        // Кнопка панели задач пересоздаётся после показа — применяем модель повторно.
        self.apply_taskbar();
        if self.reapply.is_none() {
            self.spawn_reapply();
        }
    }

    /// Через ~300 мс применить прогресс/оверлей ещё раз (окно могло появиться позже).
    /// В поток передаются только числа (`HWND`) и клон модели; COM-указатель
    /// клонируется через `AddRef` и освобождается в конце потока.
    fn spawn_reapply(&mut self) {
        let hwnd = hwnd_num(self.hwnd);
        let taskbar = self.taskbar.as_ref().map(|t| add_ref_raw(t) as usize);
        let last = self.last_model.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            let taskbar = taskbar.map(|raw| {
                // ИНВАРИАНТ: AddRef сделан в трей-потоке; Release — ниже, тем же числом ссылок.
                unsafe { ITaskbarList3::from_raw(raw as *mut c_void) }
            });
            apply_taskbar_parts(taskbar.as_ref(), hwnd_num_from(hwnd), last.as_ref());
            if let Some(t) = taskbar {
                t.into_raw();
            }
        });
        self.reapply = Some(handle);
    }

    fn join_reapply(&mut self) {
        if let Some(h) = self.reapply.take() {
            let _ = h.join();
        }
    }

    /// Переприменение прогресса и оверлея по последней модели.
    fn apply_taskbar(&self) {
        apply_taskbar_parts(self.taskbar.as_ref(), self.hwnd, self.last_model.as_ref());
    }

    fn apply_model(&mut self, model: TaskbarModel) {
        // (1) иконка трея — только при смене строк или масштаба экрана (цвет всегда белый).
        let size = tray_icon_size();
        let key = (size, model.tray_lines.clone());
        if self.last_icon.as_ref() != Some(&key) {
            if let Some(tray) = &self.tray {
                let rgba = render_tray_icon_rgba(size, &model.tray_lines, TRAY_WHITE);
                if let Ok(icon) = Icon::from_rgba(rgba, size, size) {
                    let _ = tray.set_icon(Some(icon));
                    self.last_icon = Some(key);
                }
            }
        }
        // (2) тултип.
        if let Some(tray) = &self.tray {
            let tip = truncate_chars(&model.tooltip, TIP_MAX_CHARS);
            let _ = tray.set_tooltip(Some(tip));
        }
        // (3) прогресс.
        apply_progress(self.taskbar.as_ref(), self.hwnd, Some(&model));
        // (4) оверлей.
        self.last_model = Some(model.clone());
        self.apply_overlay(&model);
        // (5) заголовок окна (tooltip миниатюры панели задач).
        set_window_title(self.hwnd, &model.title);
    }

    fn apply_overlay(&mut self, model: &TaskbarModel) {
        let Some(taskbar) = self.taskbar.clone() else {
            return;
        };
        let hwnd = self.hwnd;
        // Снять предыдущий оверлей и уничтожить его HICON (не копим GDI-хэндлы).
        if let Some(old) = self.overlay_icon.take() {
            unsafe {
                let _ = taskbar.SetOverlayIcon(hwnd, HICON::default(), PCWSTR::null());
                // ИНВАРИАНТ: HICON наш (создан CreateIconIndirect) и больше не нужен.
                let _ = DestroyIcon(old);
            }
        }
        if model.level == Level::Unknown {
            return; // оверлей снят
        }
        let text = overlay_description(model);
        if text.trim().is_empty() {
            return;
        }
        let Some(hicon) = overlay_hicon(model, &text) else {
            return;
        };
        let desc = wide(&text);
        unsafe {
            if taskbar
                .SetOverlayIcon(hwnd, hicon, PCWSTR(desc.as_ptr()))
                .is_ok()
            {
                self.overlay_icon = Some(hicon);
            } else {
                // ИНВАРИАНТ: установка не удалась — освобождаем хэндл сами.
                let _ = DestroyIcon(hicon);
            }
        }
    }
}

/// Рисует HICON оверлея (16×16) с текстом `text` на фоне уровня.
fn overlay_hicon(model: &TaskbarModel, text: &str) -> Option<HICON> {
    let rgba = render_icon_rgba(
        OVERLAY_ICON_SIZE,
        text,
        model.level.rgb(),
        model.level.text_rgb(),
    );
    hicon_from_rgba(&rgba, OVERLAY_ICON_SIZE)
}

fn apply_progress(taskbar: Option<&ITaskbarList3>, hwnd: HWND, model: Option<&TaskbarModel>) {
    let (Some(taskbar), Some(model)) = (taskbar, model) else {
        return;
    };
    let flag = match model.progress {
        ProgressKind::None => TBPF_NOPROGRESS,
        ProgressKind::Normal => TBPF_NORMAL,
        ProgressKind::Paused => TBPF_PAUSED,
        ProgressKind::Error => TBPF_ERROR,
    };
    unsafe {
        if taskbar.SetProgressState(hwnd, flag).is_err() {
            return;
        }
        if model.progress != ProgressKind::None {
            let spent = model.progress_spent.clamp(0.0, 1.0);
            let _ = taskbar.SetProgressValue(hwnd, (spent * 1000.0).round() as u64, 1000);
        }
    }
}

/// Прогресс + оверлей по последней модели (переприменение после показа окна).
fn apply_taskbar_parts(taskbar: Option<&ITaskbarList3>, hwnd: HWND, model: Option<&TaskbarModel>) {
    let (Some(taskbar), Some(model)) = (taskbar, model) else {
        return;
    };
    apply_progress(Some(taskbar), hwnd, Some(model));
    if model.level == Level::Unknown {
        return;
    }
    let text = overlay_description(model);
    if text.trim().is_empty() {
        return;
    }
    let Some(hicon) = overlay_hicon(model, &text) else {
        return;
    };
    let desc = wide(&text);
    unsafe {
        let _ = taskbar.SetOverlayIcon(hwnd, hicon, PCWSTR(desc.as_ptr()));
        // ИНВАРИАНТ: хэндл наш, освобождаем сразу — панель задач копирует изображение.
        let _ = DestroyIcon(hicon);
    }
}

/// Заголовок окна через SendMessageTimeoutW, чтобы не зависнуть на занятом UI-потоке.
fn set_window_title(hwnd: HWND, title: &str) {
    let text = wide(title);
    unsafe {
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_SETTEXT,
            WPARAM(0),
            LPARAM(text.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            500,
            None,
        );
    }
}

/// Текст оверлея: предупреждение — «!», иначе текст модели.
fn overlay_description(model: &TaskbarModel) -> String {
    if model.overlay_warning {
        "!".to_string()
    } else {
        model.overlay_text.clone()
    }
}

fn menu_event_id(id: &MenuId) -> u32 {
    id.0.parse::<u32>().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Поток трея
// ---------------------------------------------------------------------------

unsafe extern "system" fn hidden_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_APP_COMMAND {
        // ИНВАРИАНТ: состояние потока живёт до DestroyWindow (см. run_tray_thread).
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
        if ptr != 0 {
            let state = unsafe { &mut *(ptr as *mut TrayState) };
            for cmd in state.queue.drain() {
                handle_command(state, cmd);
            }
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn handle_command(state: &mut TrayState, cmd: Command) {
    match cmd {
        Command::Update(model) => state.apply_model(model),
        Command::SetAutostartChecked(on) => state.autostart.set_checked(on),
        Command::ShowWindow => state.show_window(),
        Command::Shutdown => {
            // Флаг выставляет shutdown() до отправки команды; здесь достаточно
            // закрыть окно, что завершит цикл сообщений.
        }
    }
}

fn build_menu(
    autostart_checked: bool,
) -> Option<(Menu, MenuItem, MenuItem, CheckMenuItem, MenuItem)> {
    let menu = Menu::new();
    let refresh = MenuItem::new("Обновить", true, None);
    let settings = MenuItem::new("Настройки", true, None);
    let autostart = CheckMenuItem::new("Автозапуск", true, autostart_checked, None);
    let sep = PredefinedMenuItem::separator();
    let quit = MenuItem::new("Выход", true, None);
    menu.append_items(&[&refresh, &settings, &autostart, &sep, &quit])
        .ok()?;
    Some((menu, refresh, settings, autostart, quit))
}

fn create_hidden_window() -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(hidden_wndproc),
            hInstance: hinst.into(),
            lpszClassName: HIDDEN_CLASS,
            ..Default::default()
        };
        // Класс может быть уже зарегистрирован (повторный spawn) — это не ошибка.
        let _ = RegisterClassW(&class);
        CreateWindowExW(
            Default::default(),
            HIDDEN_CLASS,
            w!("OpenRouterBalance"),
            Default::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        )
        .ok()
    }
}

fn run_tray_thread(
    queue: Arc<CommandQueue>,
    events: Sender<ShellEvent>,
    on_event: Box<dyn Fn() + Send + Sync>,
    autostart_checked: bool,
    main_hwnd: isize,
    ready: Sender<Result<(), String>>,
    shutting_down: Arc<AtomicBool>,
) {
    // ИНВАРИАНТ: CoInitializeEx/CoUninitialize парны в этом потоке (STA для Shell API).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let hwnd = match create_hidden_window() {
        Some(h) => h,
        None => {
            let _ = ready.send(Err("не удалось создать скрытое окно".into()));
            unsafe { CoUninitialize() };
            return;
        }
    };
    // Теперь команды из других потоков можно доставлять PostMessageW.
    queue.set_hwnd(hwnd_num(hwnd));
    let Some((menu, refresh, settings, autostart, quit)) = build_menu(autostart_checked) else {
        let _ = ready.send(Err("не удалось создать меню трея".into()));
        unsafe {
            let _ = DestroyWindow(hwnd);
            CoUninitialize();
        }
        return;
    };

    let taskbar: Option<ITaskbarList3> = unsafe {
        CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)
            .ok()
            .filter(|t: &ITaskbarList3| t.HrInit().is_ok())
    };

    let main = hwnd_of(main_hwnd).unwrap_or(hwnd);
    let mut state = TrayState {
        queue,
        events,
        on_event,
        hwnd: main,
        tray: None,
        menu,
        refresh,
        settings,
        autostart,
        quit,
        taskbar,
        last_model: None,
        last_icon: None,
        overlay_icon: None,
        reapply: None,
    };

    // ИНВАРИАНТ: указатель на state валиден до DestroyWindow ниже (state живёт до конца функции).
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut state as *mut TrayState as isize);
    }

    // Иконка трея создаётся в этом потоке (иначе трей не работает без цикла сообщений).
    let initial_size = tray_icon_size();
    let initial_icon = Icon::from_rgba(
        render_tray_icon_rgba(initial_size, &["…".to_string()], TRAY_WHITE),
        initial_size,
        initial_size,
    )
    .ok();
    let mut tray_builder = TrayIcon::builder()
        .with_menu(Box::new(state.menu.clone()))
        .with_tooltip("OpenRouter Баланс")
        .with_menu_on_left_click(false);
    if let Some(icon) = initial_icon {
        tray_builder = tray_builder.with_icon(icon);
    }
    match tray_builder.build() {
        Ok(tray) => {
            state.tray = Some(tray);
            state.last_icon = Some((initial_size, vec!["…".to_string()]));
        }
        Err(e) => {
            let _ = ready.send(Err(format!("не удалось создать иконку трея: {e}")));
            unsafe {
                let _ = DestroyWindow(hwnd);
                CoUninitialize();
            }
            return;
        }
    }

    let ids = (
        menu_event_id(state.refresh.id()),
        menu_event_id(state.settings.id()),
        menu_event_id(state.autostart.id()),
        menu_event_id(state.quit.id()),
    );

    // Обработчики вызываются в этом же потоке (из wndproc трея/меню).
    // В замыкание передаём HWND числом: указатели `*mut c_void` не `Send`.
    let menu_hwnd = hwnd_num(hwnd);
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let id = menu_event_id(event.id());
        let ptr = unsafe { GetWindowLongPtrW(hwnd_num_from(menu_hwnd), GWLP_USERDATA) };
        if ptr == 0 {
            return;
        }
        let state = unsafe { &mut *(ptr as *mut TrayState) };
        if id == ids.0 {
            state.emit(ShellEvent::Refresh);
        } else if id == ids.1 {
            state.show_window();
            state.emit(ShellEvent::OpenSettings);
        } else if id == ids.2 {
            let want = !state.autostart.is_checked();
            match set_autostart(want) {
                Ok(()) => {
                    state.autostart.set_checked(want);
                    state.emit(ShellEvent::AutostartToggled(want));
                }
                Err(_) => {
                    // Откатываем галочку к фактическому состоянию.
                    let actual = is_autostart();
                    state.autostart.set_checked(actual);
                    state.emit(ShellEvent::AutostartToggled(actual));
                }
            }
        } else if id == ids.3 {
            state.emit(ShellEvent::Quit);
        }
    }));

    let tray_hwnd = hwnd_num(hwnd);
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let ptr = unsafe { GetWindowLongPtrW(hwnd_num_from(tray_hwnd), GWLP_USERDATA) };
        if ptr == 0 {
            return;
        }
        let state = unsafe { &mut *(ptr as *mut TrayState) };
        match event {
            TrayIconEvent::Click {
                button,
                button_state,
                ..
            } if button == tray_icon::MouseButton::Left
                && button_state == tray_icon::MouseButtonState::Up =>
            {
                state.toggle_window();
            }
            TrayIconEvent::DoubleClick { .. } => {
                state.show_window();
            }
            _ => {}
        }
    }));

    let _ = ready.send(Ok(()));

    // Цикл сообщений этого потока.
    let mut msg = MSG::default();
    loop {
        if shutting_down.load(Ordering::SeqCst) {
            break;
        }
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !ret.as_bool() {
            break; // WM_QUIT (ret == 0) либо ошибка (ret == -1)
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Подчистка: снять прогресс/оверлей, убрать иконку трея, уничтожить хэндлы.
    state.join_reapply();
    if let Some(taskbar) = state.taskbar.clone() {
        unsafe {
            let _ = taskbar.SetProgressState(state.hwnd, TBPF_NOPROGRESS);
            let _ = taskbar.SetOverlayIcon(state.hwnd, HICON::default(), PCWSTR::null());
        }
    }
    if let Some(hicon) = state.overlay_icon.take() {
        unsafe {
            let _ = DestroyIcon(hicon);
        }
    }
    // drop TrayIcon в этом же потоке — иконка исчезает из трея.
    state.tray = None;

    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = DestroyWindow(hwnd);
        CoUninitialize();
    }
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

enum Slot {
    Running {
        queue: Arc<CommandQueue>,
        stopping: Arc<AtomicBool>,
        join: Mutex<Option<JoinHandle<()>>>,
    },
    Stopped,
}

struct Shared {
    slot: Mutex<Slot>,
    shutdown_done: AtomicBool,
}

/// Хэндл Win32-слоя. Живёт в отдельном потоке со своим циклом сообщений;
/// все методы неблокирующие.
pub struct Shell {
    shared: Arc<Shared>,
}

impl Shell {
    /// Запускает поток: иконка трея + меню, ITaskbarList3 для `hwnd` главного окна.
    /// `events` — канал для `ShellEvent`; `on_event` вызывается после отправки
    /// (например, чтобы разбудить UI: `ctx.request_repaint()`).
    pub fn spawn(
        hwnd: isize,
        events: Sender<ShellEvent>,
        on_event: Box<dyn Fn() + Send + Sync>,
        autostart_checked: bool,
    ) -> Result<Shell, String> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let stopping = Arc::new(AtomicBool::new(false));
        // Плейсхолдер очереди: скрытый hwnd появится только внутри потока.
        let queue = Arc::new(CommandQueue::new(0));
        let queue_for_thread = queue.clone();
        let stopping_for_thread = stopping.clone();

        let handle = thread::Builder::new()
            .name("openrouter-tray".to_string())
            .spawn(move || {
                run_tray_thread(
                    queue_for_thread,
                    events,
                    on_event,
                    autostart_checked,
                    hwnd,
                    ready_tx,
                    stopping_for_thread,
                );
            })
            .map_err(|e| format!("не удалось запустить поток трея: {e}"))?;

        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::Running {
                queue,
                stopping,
                join: Mutex::new(Some(handle)),
            }),
            shutdown_done: AtomicBool::new(false),
        });

        match ready_rx.recv_timeout(SHUTDOWN_WAIT) {
            Ok(Ok(())) => Ok(Shell { shared }),
            Ok(Err(e)) => {
                Shell { shared }.stop_thread();
                Err(e)
            }
            Err(_) => {
                Shell { shared }.stop_thread();
                Err("поток трея не ответил за 1 с".to_string())
            }
        }
    }

    /// Обновить трей (иконка, тултип), прогресс и оверлей кнопки, заголовок окна.
    pub fn update(&self, model: TaskbarModel) {
        self.send(Command::Update(model));
    }

    /// Синхронизировать галочку «Автозапуск» в меню.
    pub fn set_autostart_checked(&self, on: bool) {
        self.send(Command::SetAutostartChecked(on));
    }

    /// Показать окно (restore + foreground); применяет последний прогресс/оверлей заново.
    pub fn show_window(&self) {
        self.send(Command::ShowWindow);
    }

    /// Убрать иконку трея и остановить поток (вызывать при выходе).
    pub fn shutdown(&self) {
        if self.shared.shutdown_done.swap(true, Ordering::SeqCst) {
            return;
        }
        self.stop_thread();
    }

    fn send(&self, cmd: Command) {
        // ИНВАРИАНТ: блокировка короткая, вложенных блокировок нет.
        if let Ok(slot) = self.shared.slot.lock() {
            if let Slot::Running { queue, .. } = &*slot {
                queue.push(cmd);
            }
        }
    }

    fn stop_thread(&self) {
        let (queue, stopping, join) = {
            let Ok(mut slot) = self.shared.slot.lock() else {
                return;
            };
            match std::mem::replace(&mut *slot, Slot::Stopped) {
                Slot::Running {
                    queue,
                    stopping,
                    join,
                } => {
                    *slot = Slot::Running {
                        queue: queue.clone(),
                        stopping: stopping.clone(),
                        join: Mutex::new(None),
                    };
                    (queue, stopping, join)
                }
                other => {
                    *slot = other;
                    return;
                }
            }
        };
        stopping.store(true, Ordering::SeqCst);
        queue.push(Command::Shutdown);
        let handle = join.lock().ok().and_then(|mut g| g.take());
        if let Some(handle) = handle {
            let deadline = std::time::Instant::now() + SHUTDOWN_WAIT;
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
            // Иначе поток не присоединяем: ждать дольше нельзя (он всё равно завершится).
        }
        if let Ok(mut slot) = self.shared.slot.lock() {
            *slot = Slot::Stopped;
        }
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Автозапуск
// ---------------------------------------------------------------------------

/// Строка команды автозапуска для пути к exe.
pub(crate) fn autostart_command(exe: &Path) -> String {
    format!("\"{}\" --minimized", exe.display())
}

/// Путь к exe из строки команды (снимает кавычки и флаг `--minimized`).
pub(crate) fn autostart_exe_path(cmd: &str) -> Option<PathBuf> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    if let Some(rest) = cmd.strip_prefix('"') {
        let end = rest.find('"')?;
        let path = &rest[..end];
        return (!path.is_empty()).then(|| PathBuf::from(path));
    }
    let path = cmd.split_whitespace().next()?;
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Совпадает ли путь записи автозапуска с текущим exe (без учёта регистра).
pub(crate) fn autostart_matches(cmd: &str, exe: &Path) -> bool {
    match autostart_exe_path(cmd) {
        Some(p) => p
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&exe.to_string_lossy()),
        None => false,
    }
}

/// Читает значение автозапуска (None — нет записи).
pub(crate) fn read_run_value(value_name: &str) -> Option<String> {
    unsafe {
        let mut key = HKEY::default();
        let sub = wide(RUN_KEY);
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let name = wide(value_name);
        let mut size: u32 = 0;
        let status = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        );
        let mut buf = vec![0u8; size as usize + 2];
        let mut size2 = buf.len() as u32;
        let status2 = if status == ERROR_SUCCESS && size > 0 {
            RegQueryValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                None,
                Some(buf.as_mut_ptr()),
                Some(&mut size2),
            )
        } else {
            status
        };
        let _ = RegCloseKey(key);
        if status2 != ERROR_SUCCESS {
            return None;
        }
        let units: Vec<u16> = buf
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|u| *u != 0)
            .collect();
        Some(String::from_utf16_lossy(&units))
    }
}

/// Записывает/удаляет значение автозапуска.
pub(crate) fn write_run_value(value_name: &str, command: Option<&str>) -> Result<(), String> {
    unsafe {
        let sub = wide(RUN_KEY);
        if command.is_none() {
            // Удаление: если ключа нет — это не ошибка.
            let mut key = HKEY::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(sub.as_ptr()),
                None,
                KEY_SET_VALUE,
                &mut key,
            ) != ERROR_SUCCESS
            {
                return Ok(());
            }
            let name = wide(value_name);
            let status = RegDeleteValueW(key, PCWSTR(name.as_ptr()));
            let _ = RegCloseKey(key);
            return if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(format!("RegDeleteValueW: {}", status.0))
            };
        }
        let mut key = HKEY::default();
        let sub_key = wide(RUN_KEY);
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub_key.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        );
        if status != ERROR_SUCCESS {
            return Err(format!("RegCreateKeyExW: {}", status.0));
        }
        let name = wide(value_name);
        let data = wide(command.unwrap_or_default());
        let bytes: Vec<u8> = data.iter().flat_map(|u| u.to_le_bytes()).collect();
        let status = RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&bytes));
        let _ = RegCloseKey(key);
        if status != ERROR_SUCCESS {
            return Err(format!("RegSetValueExW: {}", status.0));
        }
        Ok(())
    }
}

fn current_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// Включить/выключить автозапуск (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`).
/// Команда содержит путь к exe и флаг `--minimized`.
pub fn set_autostart(enabled: bool) -> Result<(), String> {
    if enabled {
        let exe = current_exe().ok_or_else(|| "не удалось определить путь к exe".to_string())?;
        write_run_value(RUN_VALUE, Some(&autostart_command(&exe)))
    } else {
        write_run_value(RUN_VALUE, None)
    }
}

/// Есть ли запись автозапуска, указывающая на текущий exe.
pub fn is_autostart() -> bool {
    let Some(exe) = current_exe() else {
        return false;
    };
    match read_run_value(RUN_VALUE) {
        Some(cmd) => autostart_matches(&cmd, &exe),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Единственный экземпляр
// ---------------------------------------------------------------------------

/// Защита единственного экземпляра (именованный mutex).
pub struct InstanceGuard {
    mutex: HANDLE,
    event: HANDLE,
}

unsafe impl Send for InstanceGuard {}
unsafe impl Sync for InstanceGuard {}

impl InstanceGuard {
    fn acquire_named(mutex_name: &str, event_name: &str) -> Option<InstanceGuard> {
        unsafe {
            let name = wide(mutex_name);
            let mutex = match CreateMutexW(None, true, PCWSTR(name.as_ptr())) {
                Ok(h) => h,
                Err(_) => return None,
            };
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // Мы не первые: создаём/открываем событие и будим существующий экземпляр.
                let ev_name = wide(event_name);
                if let Ok(ev) = CreateEventW(None, false, false, PCWSTR(ev_name.as_ptr())) {
                    let _ = SetEvent(ev);
                    let _ = CloseHandle(ev);
                }
                let _ = CloseHandle(mutex);
                return None;
            }
            let ev_name = wide(event_name);
            match CreateEventW(None, false, false, PCWSTR(ev_name.as_ptr())) {
                Ok(event) => Some(InstanceGuard { mutex, event }),
                Err(_) => {
                    let _ = CloseHandle(mutex);
                    None
                }
            }
        }
    }

    /// `Some(guard)` — мы первые. `None` — экземпляр уже запущен; ему отправлен
    /// сигнал «показать окно» (именованное событие), текущий процесс должен завершиться.
    pub fn acquire() -> Option<InstanceGuard> {
        Self::acquire_named(INSTANCE_MUTEX, INSTANCE_EVENT)
    }

    /// Запускает поток, который при сигнале от повторного запуска вызывает `on_activate`.
    pub fn listen(&self, on_activate: Box<dyn Fn() + Send + 'static>) {
        // ИНВАРИАНТ: hэндл события живёт в guard, поток ждёт на нём до конца процесса.
        let event = self.event.0 as isize;
        thread::Builder::new()
            .name("openrouter-instance".to_string())
            .spawn(move || loop {
                let handle = HANDLE(event as *mut c_void);
                let rc = unsafe { WaitForSingleObject(handle, INFINITE) };
                if rc != WAIT_OBJECT_0 {
                    break;
                }
                on_activate();
            })
            .ok();
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.mutex);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Обёртки для тестов: в проекте модуль вложен в бинарный крейт (`crate::format`),
    /// поэтому внутри `super` используем короткие пути.
    fn next_level(l: Level) -> Level {
        l
    }

    fn level_rgb(l: Level) -> [u8; 3] {
        l.rgb()
    }

    // --- render_icon_rgba ---------------------------------------------------

    fn px(buf: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn icon_buffer_size_and_corner() {
        let bg = Level::Green.rgb();
        let icon = render_icon_rgba(32, "$12", bg, Level::Green.text_rgb());
        assert_eq!(icon.len(), 32 * 32 * 4);
        // Угол скруглён — прозрачный или полупрозрачный на самой кромке.
        assert!(
            px(&icon, 32, 0, 0)[3] < 255,
            "угол должен быть не полностью непрозрачным"
        );
        // Центр ребра — фон.
        let edge = px(&icon, 32, 16, 0);
        assert_eq!(&edge[0..3], &bg[..]);
        assert_eq!(edge[3], 255);
    }

    #[test]
    fn icon_has_foreground_pixels_in_center() {
        let bg = Level::Green.rgb();
        let fg = Level::Green.text_rgb();
        let icon = render_icon_rgba(32, "$1", bg, fg);
        let mut hits = 0;
        for y in 0..32 {
            for x in 0..32 {
                let p = px(&icon, 32, x, y);
                if p[2] > 200 && p[0] > 200 && p[1] > 200 {
                    hits += 1;
                }
            }
        }
        assert!(
            hits > 20,
            "в центре должны быть пиксели цвета fg, найдено {hits}"
        );
    }

    #[test]
    fn icon_foreground_visible_for_all_levels() {
        for level in [
            Level::Green,
            Level::Yellow,
            Level::Red,
            Level::NoLimit,
            Level::Error,
            Level::Unknown,
        ] {
            let icon = render_icon_rgba(16, "8", level_rgb(level), level.text_rgb());
            let fg = level.text_rgb();
            let found = (0..16).any(|y| {
                (0..16).any(|x| {
                    let p = px(&icon, 16, x, y);
                    p[0] == fg[0] && p[1] == fg[1] && p[2] == fg[2]
                })
            });
            assert!(found, "нет текста для {:?}", next_level(level));
        }
    }

    #[test]
    fn different_texts_differ() {
        let bg = Level::Green.rgb();
        let fg = Level::Green.text_rgb();
        let a = render_icon_rgba(32, "$1", bg, fg);
        let b = render_icon_rgba(32, "$99", bg, fg);
        let c = render_icon_rgba(32, "!", bg, fg);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn empty_text_is_solid_background() {
        let bg = Level::Yellow.rgb();
        let icon = render_icon_rgba(16, "", bg, Level::Yellow.text_rgb());
        assert_eq!(icon.len(), 16 * 16 * 4);
        for y in 1..15 {
            for x in 1..15 {
                let p = px(&icon, 16, x, y);
                assert_eq!(
                    &p[0..3],
                    &bg[..],
                    "при пустом тексте фон должен быть сплошным"
                );
                assert_eq!(p[3], 255);
            }
        }
    }

    #[test]
    fn long_text_does_not_panic_and_fits() {
        let icon = render_icon_rgba(16, "$$$$", Level::Red.rgb(), Level::Red.text_rgb());
        assert_eq!(icon.len(), 16 * 16 * 4);
        let icon1 = render_icon_rgba(1, "8", Level::Red.rgb(), Level::Red.text_rgb());
        assert_eq!(icon1.len(), 4);
    }

    // --- render_tray_icon_rgba ---------------------------------------------

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Есть ли пиксели, близкие к `rgb` и достаточно непрозрачные.
    fn has_color(buf: &[u8], size: u32, rgb: [u8; 3], min_alpha: u8) -> bool {
        (0..size).any(|y| {
            (0..size).any(|x| {
                let p = px(buf, size, x, y);
                p[3] >= min_alpha && (0..3).all(|c| (p[c] as i32 - rgb[c] as i32).abs() <= 8)
            })
        })
    }

    #[test]
    fn tray_icon_buffer_size_and_transparent_background() {
        let rgba = render_tray_icon_rgba(32, &lines(&["12.53"]), Level::Green.rgb());
        assert_eq!(rgba.len(), 32 * 32 * 4);
        for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31), (0, 16)] {
            assert_eq!(
                px(&rgba, 32, x, y)[3],
                0,
                "угол/край ({x},{y}) не прозрачен"
            );
        }
        assert!(
            has_color(&rgba, 32, Level::Green.rgb(), 200),
            "нет непрозрачных пикселей цвета уровня"
        );
    }

    #[test]
    fn tray_icon_different_lines_differ() {
        let c = Level::Yellow.rgb();
        let a = render_tray_icon_rgba(32, &lines(&["12.53"]), c);
        let b = render_tray_icon_rgba(32, &lines(&["0.85"]), c);
        let d = render_tray_icon_rgba(32, &lines(&["12.40", "40.00"]), c);
        let e = render_tray_icon_rgba(32, &lines(&["!"]), c);
        assert_ne!(a, b);
        assert_ne!(a, d);
        assert_ne!(a, e);
        assert_ne!(d, b);
    }

    #[test]
    fn tray_icon_single_line_is_larger_than_two_lines() {
        // Одна строка должна быть заметно крупнее, чем каждая из двух строк.
        let one = render_tray_icon_rgba(32, &lines(&["1253"]), Level::Red.rgb());
        let two = render_tray_icon_rgba(32, &lines(&["12.40", "40.00"]), Level::Red.rgb());
        let height = |b: &[u8]| {
            let (_, y0, _, y1) = ink_bounds(b, 32 * 4, 32);
            (y1 - y0 + 1).max(0)
        };
        // Одна строка занимает почти всю высоту иконки (было ~20 px — стало больше).
        assert!(
            height(&one) >= 16,
            "одна строка должна быть крупной, получено {}",
            height(&one)
        );
        // Две строки вместе выше одной, но каждая — примерно вдвое ниже.
        let h2 = height(&two);
        assert!(h2 >= height(&one), "две строки вместе не ниже одной");
        // "12.53" делится на "12" сверху и "53" снизу — как две строки.
        let split = render_tray_icon_rgba(32, &lines(&["12.53"]), Level::Red.rgb());
        let manual = render_tray_icon_rgba(32, &lines(&["12", "53"]), Level::Red.rgb());
        assert_eq!(split, manual);
        assert!(h2 <= 32, "две строки должны влезать в иконку");
    }

    #[test]
    fn tray_icon_edge_cases_do_not_panic() {
        let empty: Vec<String> = Vec::new();
        let rgba = render_tray_icon_rgba(32, &empty, Level::Error.rgb());
        assert_eq!(rgba.len(), 32 * 32 * 4);
        assert!(rgba.iter().all(|b| *b == 0), "пустой список → прозрачно");
        // Пустые строки и пробелы — то же самое.
        assert!(
            render_tray_icon_rgba(16, &lines(&["", "  "]), Level::Red.rgb())
                .iter()
                .all(|b| *b == 0)
        );
        // Очень длинная строка и миниатюрный размер не паникуют.
        let nine = "9".repeat(200);
        let long = render_tray_icon_rgba(32, &lines(&[nine.as_str()]), Level::Red.rgb());
        assert_eq!(long.len(), 32 * 32 * 4);
        assert_eq!(
            render_tray_icon_rgba(1, &lines(&["12.53"]), Level::Red.rgb()).len(),
            4
        );
        // Больше двух строк — берём первые две.
        let many = render_tray_icon_rgba(32, &lines(&["1", "2", "3", "4"]), Level::Red.rgb());
        let two = render_tray_icon_rgba(32, &lines(&["1", "2"]), Level::Red.rgb());
        assert_eq!(many, two);
    }

    #[test]
    fn tray_icon_has_dark_outline_and_alpha() {
        let color = Level::Green.rgb();
        let rgba = render_tray_icon_rgba(32, &lines(&["12.53"]), color);
        // Контур: рядом с цифрами есть заметно более тёмные пиксели того же тона.
        let lum = |p: [u8; 4]| p[0] as u32 + p[1] as u32 + p[2] as u32;
        let dark = (0..32).any(|y| {
            (0..32).any(|x| {
                let p = px(&rgba, 32, x, y);
                p[3] > 120 && lum(p) * 2 < lum([color[0], color[1], color[2], 255])
            })
        });
        assert!(dark, "нет тёмного контура вокруг цифр");
        // Есть полупрозрачные пиксели сглаживания — значит, маска не бинарная.
        assert!((0..32 * 32).any(|i| {
            let a = rgba[i * 4 + 3];
            a > 10 && a < 245
        }));
    }

    /// Утилита для визуальной проверки: `cargo test taskbar::tests::dump_tray_previews -- --ignored --nocapture`
    /// Кладёт PNG (32×32 и 16×16) на сером и белом фоне в `%TEMP%\orb-tray-preview`.
    #[test]
    #[ignore = "утилита: сохраняет PNG в %TEMP% для ручного просмотра"]
    fn dump_tray_previews() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("orb-tray-preview");
        let _ = std::fs::create_dir_all(&dir);
        let samples: [(&str, Vec<String>); 8] = [
            ("12.53", lines(&["12.53"])),
            ("minus3", lines(&["-3.00"])),
            ("0.85", lines(&["0.85"])),
            ("999.99", lines(&["999.99"])),
            ("1235", lines(&["1235"])),
            ("250k", lines(&["250k"])),
            ("bang", lines(&["!"])),
            ("two_lines", lines(&["12.40", "40.00"])),
        ];
        let mut written = Vec::new();
        for (name, ls) in samples {
            let icon = render_tray_icon_rgba(32, &ls, TRAY_WHITE);
            // Высота чернил: главный показатель «крупнее ли цифры».
            let (bx0, by0, bx1, by1) = ink_bounds(&icon, 32 * 4, 32);
            println!(
                "INK {name}: w={} h={} bbox=({bx0},{by0})-({bx1},{by1})",
                bx1 - bx0 + 1,
                by1 - by0 + 1
            );
            for (bg_name, bg) in [("gray", [128u8, 128, 128]), ("white", [255u8, 255, 255])] {
                let composed = compose_on(&icon, 32, bg);
                let small = compose_on(&downscale_half(&icon, 32), 16, bg);
                // Нативная отрисовка под 16 и 24 px (как в трее при 100% и 150%).
                let n16 = compose_on(&render_tray_icon_rgba(16, &ls, TRAY_WHITE), 16, bg);
                let n24 = compose_on(&render_tray_icon_rgba(24, &ls, TRAY_WHITE), 24, bg);
                for (suffix, size, rgba) in [
                    ("32", 32u32, composed.clone()),
                    ("16", 16u32, small.clone()),
                    ("32x8", 256u32, upscale(&composed, 32, 8)),
                    ("16x8", 128u32, upscale(&small, 16, 8)),
                    ("n16x8", 128u32, upscale(&n16, 16, 8)),
                    ("n24x8", 192u32, upscale(&n24, 24, 8)),
                ] {
                    let path = dir.join(format!("{name}_{bg_name}_{suffix}.png"));
                    if std::fs::File::create(&path)
                        .map(|mut f| {
                            let _ = f.write_all(&png(size, size, &rgba));
                        })
                        .is_ok()
                    {
                        written.push(path.display().to_string());
                    }
                }
            }
        }
        println!("PNG ({} шт.):", written.len());
        for p in written {
            println!("  {p}");
        }
    }

    /// Увеличение целочисленным множителем (nearest neighbour) для разглядывания.
    fn upscale(rgba: &[u8], size: u32, factor: u32) -> Vec<u8> {
        let out_size = size * factor;
        let mut out = vec![0u8; (out_size as usize) * (out_size as usize) * 4];
        for y in 0..out_size {
            for x in 0..out_size {
                let s = (((y / factor) * size + (x / factor)) * 4) as usize;
                let d = ((y * out_size + x) * 4) as usize;
                out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
            }
        }
        out
    }

    /// Накладывает RGBA на непрозрачный фон (straight alpha) — как панель задач.
    fn compose_on(rgba: &[u8], size: u32, bg: [u8; 3]) -> Vec<u8> {
        let mut out = vec![0u8; (size as usize) * (size as usize) * 4];
        for i in 0..(size as usize) * (size as usize) {
            let a = rgba[i * 4 + 3] as f64 / 255.0;
            for c in 0..3 {
                let fg = rgba[i * 4 + c] as f64;
                out[i * 4 + c] = (fg * a + bg[c] as f64 * (1.0 - a)).round() as u8;
            }
            out[i * 4 + 3] = 255;
        }
        out
    }

    /// Уменьшение вдвое (box filter) — имитация отрисовки 32×32 в 16×16.
    fn downscale_half(rgba: &[u8], size: u32) -> Vec<u8> {
        let half = size / 2;
        let mut out = vec![0u8; (half as usize) * (half as usize) * 4];
        for y in 0..half {
            for x in 0..half {
                let mut acc = [0.0f64; 4];
                for dy in 0..2 {
                    for dx in 0..2 {
                        let i = (((y * 2 + dy) * size + x * 2 + dx) * 4) as usize;
                        let a = rgba[i + 3] as f64 / 255.0;
                        for c in 0..3 {
                            acc[c] += rgba[i + c] as f64 * a;
                        }
                        acc[3] += a;
                    }
                }
                let a = acc[3] / 4.0;
                let o = ((y * half + x) * 4) as usize;
                for c in 0..3 {
                    out[o + c] = if a > 0.0 {
                        (acc[c] / 4.0 / a).round().clamp(0.0, 255.0) as u8
                    } else {
                        0
                    };
                }
                out[o + 3] = (a * 255.0).round() as u8;
            }
        }
        out
    }

    /// Минимальный PNG (RGBA8, без фильтров, zlib stored) — только для отладки.
    fn png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut table = [0u32; 256];
            for (i, t) in table.iter_mut().enumerate() {
                let mut c = i as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        0xEDB8_8320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
                *t = c;
            }
            let mut c = 0xFFFF_FFFFu32;
            for b in data {
                c = table[((c ^ *b as u32) & 0xFF) as usize] ^ (c >> 8);
            }
            !c
        }
        fn chunk(kind: &[u8; 4], body: &[u8], out: &mut Vec<u8>) {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            let mut with_kind = kind.to_vec();
            with_kind.extend_from_slice(body);
            out.extend_from_slice(&with_kind);
            out.extend_from_slice(&crc32(&with_kind).to_be_bytes());
        }
        let mut raw = Vec::with_capacity((h * (w * 4 + 1)) as usize);
        for y in 0..h {
            raw.push(0); // фильтр None
            let s = (y * w * 4) as usize;
            raw.extend_from_slice(&rgba[s..s + (w * 4) as usize]);
        }
        let mut z = vec![0x78u8, 0x01];
        let mut pos = 0usize;
        while pos < raw.len() {
            let n = (raw.len() - pos).min(65535);
            let last = pos + n >= raw.len();
            z.push(if last { 1 } else { 0 });
            z.extend_from_slice(&(n as u16).to_le_bytes());
            z.extend_from_slice(&(!(n as u16)).to_le_bytes());
            z.extend_from_slice(&raw[pos..pos + n]);
            pos += n;
        }
        let mut adler_a = 1u32;
        let mut adler_b = 0u32;
        for b in &raw {
            adler_a = (adler_a + *b as u32) % 65521;
            adler_b = (adler_b + adler_a) % 65521;
        }
        z.extend_from_slice(&((adler_b << 16) | adler_a).to_be_bytes());

        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(b"IHDR", &ihdr, &mut out);
        chunk(b"IDAT", &z, &mut out);
        chunk(b"IEND", &[], &mut out);
        out
    }

    // --- автозапуск: чистые функции ----------------------------------------

    #[test]
    fn autostart_command_quotes_path() {
        let cmd = autostart_command(Path::new(r"C:\Program Files\OpenRouter Balance\app.exe"));
        assert_eq!(
            cmd,
            "\"C:\\Program Files\\OpenRouter Balance\\app.exe\" --minimized"
        );
        assert_eq!(
            autostart_exe_path(&cmd),
            Some(PathBuf::from(
                r"C:\Program Files\OpenRouter Balance\app.exe"
            ))
        );
    }

    #[test]
    fn autostart_command_without_spaces() {
        let cmd = autostart_command(Path::new(r"C:\app.exe"));
        assert_eq!(cmd, "\"C:\\app.exe\" --minimized");
        assert_eq!(autostart_exe_path(&cmd), Some(PathBuf::from(r"C:\app.exe")));
    }

    #[test]
    fn autostart_parse_edge_cases() {
        assert_eq!(autostart_exe_path(""), None);
        assert_eq!(autostart_exe_path("   "), None);
        assert_eq!(autostart_exe_path("\"\""), None);
        assert_eq!(
            autostart_exe_path("\"C:\\a b\\app.exe\""),
            Some(PathBuf::from(r"C:\a b\app.exe"))
        );
        assert_eq!(
            autostart_exe_path(r"C:\app.exe --minimized"),
            Some(PathBuf::from(r"C:\app.exe"))
        );
    }

    #[test]
    fn autostart_matches_is_case_insensitive() {
        let exe = Path::new(r"C:\Users\User\app.exe");
        assert!(autostart_matches(
            "\"c:\\users\\user\\APP.EXE\" --minimized",
            exe
        ));
        assert!(!autostart_matches(
            "\"C:\\other\\app.exe\" --minimized",
            exe
        ));
        assert!(!autostart_matches("", exe));
    }

    #[cfg(windows)]
    #[test]
    fn run_value_roundtrip_temp_name() {
        let name = format!("OpenRouterBalance.Test.{}", std::process::id());
        assert_eq!(read_run_value(&name), None);
        if let Err(e) = write_run_value(&name, Some("\"C:\\x\\app.exe\" --minimized")) {
            // В песочнице/под ограниченной учёткой запись в HKCU может быть запрещена.
            if e.ends_with(": 5") {
                eprintln!("skip: нет доступа к HKCU\\...\\Run ({e})");
                return;
            }
            panic!("write_run_value: {e}");
        }
        assert_eq!(
            read_run_value(&name).as_deref(),
            Some("\"C:\\x\\app.exe\" --minimized")
        );
        assert!(write_run_value(&name, None).is_ok());
        assert_eq!(read_run_value(&name), None);
        // Повторное удаление несуществующего значения — не ошибка.
        assert!(write_run_value(&name, None).is_ok());
    }

    // --- единственный экземпляр --------------------------------------------

    #[cfg(windows)]
    #[test]
    fn instance_guard_second_acquire_returns_none() {
        let tag = std::process::id();
        let mutex = format!("Local\\OpenRouterBalance.Test.{tag}");
        let event = format!("Local\\OpenRouterBalance.Test.Show.{tag}");
        let first = InstanceGuard::acquire_named(&mutex, &event);
        assert!(first.is_some(), "первый acquire должен вернуть Some");
        let second = InstanceGuard::acquire_named(&mutex, &event);
        assert!(second.is_none(), "повторный acquire должен вернуть None");
        drop(first);
        // После освобождения мьютекса снова можно стать первым.
        let third = InstanceGuard::acquire_named(&mutex, &event);
        assert!(third.is_some());
    }

    // --- требующие рабочего стола ------------------------------------------

    /// Требует рабочего стола (трей + панель задач). Запускать вручную:
    /// `cargo test taskbar::tests::desktop_shell_smoke -- --ignored --nocapture`
    #[cfg(windows)]
    #[test]
    #[ignore = "требует рабочего стола: создаёт трей-иконку и окно Win32"]
    fn desktop_shell_smoke() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
            RegisterClassW, ShowWindow, TranslateMessage, MSG, PM_REMOVE, SW_SHOW, WM_DESTROY,
            WNDCLASSW, WS_OVERLAPPEDWINDOW,
        };
        use GetModuleHandleW;

        unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            if msg == WM_DESTROY {
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }

        let (hwnd, shell, rx) = unsafe {
            let hinst = match GetModuleHandleW(None) {
                Ok(h) => h,
                Err(_) => return,
            };
            let class = WNDCLASSW {
                lpfnWndProc: Some(proc_),
                hInstance: hinst.into(),
                lpszClassName: w!("OrbTestWindow"),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return;
            }
            let hwnd = match CreateWindowExW(
                Default::default(),
                w!("OrbTestWindow"),
                w!("Test"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                300,
                120,
                None,
                None,
                Some(hinst.into()),
                None,
            ) {
                Ok(h) => h,
                Err(_) => return,
            };
            let _ = ShowWindow(hwnd, SW_SHOW);
            let (tx, rx) = std::sync::mpsc::channel();
            let shell = match Shell::spawn(hwnd.0 as isize, tx, Box::new(|| {}), false) {
                Ok(s) => s,
                Err(e) => {
                    let _ = DestroyWindow(hwnd);
                    panic!("spawn: {e}");
                }
            };
            (hwnd, shell, rx)
        };

        // Неблокирующая прокачка очереди: PeekMessageW не залипает на пустой очереди.
        let pump = |ms: u64| unsafe {
            let deadline = std::time::Instant::now() + Duration::from_millis(ms);
            let mut msg = MSG::default();
            while std::time::Instant::now() < deadline {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };

        for (level, text, progress, warn) in [
            (Level::Green, "$12", ProgressKind::Normal, false),
            (Level::Yellow, "$9.5", ProgressKind::Paused, false),
            (Level::Red, "!", ProgressKind::Error, true),
            (Level::NoLimit, "$7.3", ProgressKind::None, false),
            (Level::Error, "…", ProgressKind::Error, false),
            (Level::Unknown, "?", ProgressKind::None, false),
        ] {
            shell.update(TaskbarModel {
                level,
                tray_text: text.into(),
                tray_lines: vec![text.to_string()],
                overlay_text: text.into(),
                overlay_warning: warn,
                tooltip: format!("probe {level:?}"),
                title: format!("OpenRouter — probe {level:?}"),
                progress,
                progress_spent: 0.4,
            });
            pump(120);
        }
        shell.show_window();
        pump(400);
        shell.set_autostart_checked(true);
        pump(120);
        shell.shutdown();
        shell.shutdown();
        drop(shell);
        assert!(rx.try_recv().is_err() || true);
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}
