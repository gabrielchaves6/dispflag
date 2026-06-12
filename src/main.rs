#![windows_subsystem = "windows"]

use std::mem;
use windows::core::PCWSTR;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// Raw flag values not stable in windows-rs 0.58
const QDC_VIRTUAL_MODE_AWARE: u32 = 0x0000_0010;
const SDC_VIRTUAL_MODE_AWARE: u32 = 0x8000_0000;
const SDC_TOPOLOGY_INTERNAL_V: u32 = 0x0000_0001;
const SDC_APPLY_V: u32 = 0x0000_0080;
const SDC_USE_SUPPLIED_V: u32 = 0x0000_0020;
const SDC_SAVE_TO_DB_V: u32 = 0x0000_0100;

const WM_TRAY: u32 = WM_APP + 1;
const CMD_TOGGLE: usize = 1;
const CMD_EXIT: usize = 2;

static mut G_MODE: u32 = 0; // 0 = native HD, 1 = 4K virtual

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | ((g as u32) << 8) | ((b as u32) << 16))
}

// ---------- Registry ----------

unsafe fn reg_read() -> u32 {
    let path = w("Software\\DispFlag");
    let nm = w("Mode");
    let mut hk = HKEY::default();
    if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), 0, KEY_READ, &mut hk).is_ok() {
        let mut val = 0u32;
        let mut sz = 4u32;
        let ok = RegQueryValueExW(
            hk, PCWSTR(nm.as_ptr()), None, None,
            Some(&mut val as *mut _ as *mut u8), Some(&mut sz),
        ).is_ok();
        let _ = RegCloseKey(hk);
        if ok { return val; }
    }
    0
}

unsafe fn reg_write(val: u32) {
    let path = w("Software\\DispFlag");
    let nm = w("Mode");
    let mut hk = HKEY::default();
    if RegCreateKeyW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), &mut hk).is_ok() {
        let _ = RegSetValueExW(hk, PCWSTR(nm.as_ptr()), 0, REG_DWORD, Some(&val.to_le_bytes()));
        let _ = RegCloseKey(hk);
    }
}

// ---------- Display switching ----------

// Scan EnumDisplaySettings for a mode >= 3840x2160 (present when NVIDIA DSR is enabled)
unsafe fn find_4k_devmode() -> Option<DEVMODEW> {
    let mut i = 0u32;
    let mut dm: DEVMODEW = mem::zeroed();
    dm.dmSize = mem::size_of::<DEVMODEW>() as u16;
    while EnumDisplaySettingsW(PCWSTR::null(), ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm).as_bool() {
        if dm.dmPelsWidth >= 3840 && dm.dmPelsHeight >= 2160 {
            return Some(dm);
        }
        i += 1;
    }
    None
}

unsafe fn apply_4k() -> bool {
    // Primary: ChangeDisplaySettingsExW with an enumerated DSR/virtual 4K mode
    if let Some(dm) = find_4k_devmode() {
        if ChangeDisplaySettingsExW(PCWSTR::null(), Some(&dm), None, CDS_UPDATEREGISTRY, None)
            == DISP_CHANGE_SUCCESSFUL
        {
            return true;
        }
    }

    // Fallback: SetDisplayConfig virtual-mode API
    let qf = QUERY_DISPLAY_CONFIG_FLAGS(QDC_ONLY_ACTIVE_PATHS.0 | QDC_VIRTUAL_MODE_AWARE);
    let mut np = 0u32;
    let mut nm = 0u32;
    if GetDisplayConfigBufferSizes(qf, &mut np, &mut nm) != ERROR_SUCCESS {
        return false;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
    if QueryDisplayConfig(qf, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
        != ERROR_SUCCESS
    {
        return false;
    }
    for m in &mut modes[..nm as usize] {
        if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            m.Anonymous.sourceMode.width = 3840;
            m.Anonymous.sourceMode.height = 2160;
        }
    }
    let sf = SET_DISPLAY_CONFIG_FLAGS(
        SDC_APPLY_V | SDC_USE_SUPPLIED_V | SDC_SAVE_TO_DB_V | SDC_VIRTUAL_MODE_AWARE,
    );
    SetDisplayConfig(
        Some(&paths[..np as usize]),
        Some(&modes[..nm as usize]),
        sf,
    ) == 0
}

unsafe fn apply_native() -> bool {
    // Reset primary display to its default (native) resolution
    if ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_UPDATEREGISTRY, None)
        == DISP_CHANGE_SUCCESSFUL
    {
        return true;
    }
    let sf = SET_DISPLAY_CONFIG_FLAGS(SDC_APPLY_V | SDC_TOPOLOGY_INTERNAL_V | SDC_SAVE_TO_DB_V);
    SetDisplayConfig(None, None, sf) == 0
}

// ---------- Tray icon (GDI badge) ----------

unsafe fn make_icon(mode: u32) -> HICON {
    let sz = 32i32;
    let sdc = GetDC(None);
    let dc = CreateCompatibleDC(sdc);
    let bmp = CreateCompatibleBitmap(sdc, sz, sz);
    ReleaseDC(None, sdc);
    let prev = SelectObject(dc, bmp);

    let bg = if mode == 1 { rgb(0, 190, 90) } else { rgb(85, 85, 85) };
    let br = CreateSolidBrush(bg);
    let rc = RECT { left: 0, top: 0, right: sz, bottom: sz };
    FillRect(dc, &rc, br);
    DeleteObject(br);

    let fn_w = w("Segoe UI");
    let font = CreateFontW(
        16, 0, 0, 0, FW_BOLD.0 as i32,
        0, 0, 0, DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        PCWSTR(fn_w.as_ptr()),
    );
    let prev_font = SelectObject(dc, font);
    SetTextColor(dc, rgb(255, 255, 255));
    SetBkMode(dc, TRANSPARENT);
    let mut lbl: Vec<u16> = (if mode == 1 { "4K" } else { "HD" }).encode_utf16().collect();
    let mut r = rc;
    DrawTextW(dc, &mut lbl, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(dc, prev_font);
    DeleteObject(font);
    SelectObject(dc, prev);

    let mask = CreateBitmap(sz, sz, 1, 1, None);
    let ii = ICONINFO {
        fIcon: BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: bmp,
    };
    let ico = CreateIconIndirect(&ii).unwrap_or_default();
    DeleteObject(mask);
    DeleteObject(bmp);
    DeleteDC(dc);
    ico
}

unsafe fn tray_op(hwnd: HWND, op: NOTIFY_ICON_MESSAGE) {
    let mut nid: NOTIFYICONDATAW = mem::zeroed();
    nid.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;

    if op == NIM_DELETE {
        Shell_NotifyIconW(op, &nid);
        return;
    }

    let ico = make_icon(G_MODE);
    nid.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = ico;
    let tip = if G_MODE == 1 { "DispFlag \u{2014} 4K Virtual active" } else { "DispFlag \u{2014} Native (HD)" };
    let tv: Vec<u16> = tip.encode_utf16().chain(Some(0)).collect();
    let n = tv.len().min(128);
    nid.szTip[..n].copy_from_slice(&tv[..n]);
    Shell_NotifyIconW(op, &nid);
    DestroyIcon(ico);
}

// ---------- Menu & toggle ----------

unsafe fn show_menu(hwnd: HWND) {
    let hm = CreatePopupMenu().unwrap();
    let lbl = if G_MODE == 1 { "Switch to HD (Native)" } else { "Switch to 4K Virtual" };
    let lw = w(lbl);
    let ew = w("Exit");
    let _ = AppendMenuW(hm, MF_STRING, CMD_TOGGLE, PCWSTR(lw.as_ptr()));
    let _ = AppendMenuW(hm, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(hm, MF_STRING, CMD_EXIT, PCWSTR(ew.as_ptr()));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    TrackPopupMenu(hm, TPM_RIGHTALIGN | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, None);
    let _ = DestroyMenu(hm);
}

unsafe fn do_toggle(hwnd: HWND) {
    let next = 1 - G_MODE;
    let ok = if next == 1 { apply_4k() } else { apply_native() };
    if ok {
        G_MODE = next;
        reg_write(next);
        tray_op(hwnd, NIM_MODIFY);
    } else {
        let msg_w = w(concat!(
            "Could not switch to 4K virtual resolution.\n\n",
            "Your laptop has an NVIDIA Quadro P620. You need to enable DSR first:\n\n",
            "1. Open NVIDIA Control Panel\n",
            "2. Manage 3D Settings \u{2192} DSR - Factors \u{2192} check \u{22184}x (4.00x)\n",
            "3. Click Apply\n\n",
            "Then try again \u{2014} the 3840\u{d7}2160 mode will appear and DispFlag will use it."
        ));
        let title_w = w("DispFlag");
        MessageBoxW(hwnd, PCWSTR(msg_w.as_ptr()), PCWSTR(title_w.as_ptr()), MB_ICONWARNING | MB_OK);
    }
}

// ---------- Window procedure ----------

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                tray_op(hwnd, NIM_ADD);
                LRESULT(0)
            }
            WM_DESTROY => {
                tray_op(hwnd, NIM_DELETE);
                PostQuitMessage(0);
                LRESULT(0)
            }
            m if m == WM_TRAY => {
                match (lp.0 & 0xFFFF) as u32 {
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    WM_LBUTTONDBLCLK => do_toggle(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wp.0 & 0xFFFF {
                    CMD_TOGGLE => do_toggle(hwnd),
                    CMD_EXIT => { let _ = DestroyWindow(hwnd); }
                    _ => {}
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ---------- Entry point ----------

fn main() {
    unsafe {
        let mn = w("DispFlagSingleInstance");
        let mtx = CreateMutexW(None, BOOL(1), PCWSTR(mn.as_ptr())).unwrap_or_default();
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if !mtx.is_invalid() { let _ = CloseHandle(mtx); }
            return;
        }

        G_MODE = reg_read();

        let hmod = GetModuleHandleW(None).unwrap();
        let hinst = HINSTANCE(hmod.0);
        let cn = w("DispFlagWnd");
        let wc = WNDCLASSEXW {
            cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            lpszClassName: PCWSTR(cn.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let tn = w("DispFlag");
        let _hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE,
            PCWSTR(cn.as_ptr()),
            PCWSTR(tn.as_ptr()),
            WS_OVERLAPPED,
            0, 0, 0, 0,
            None, None, hmod, None,
        )
        .unwrap();

        if G_MODE == 1 { apply_4k(); }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if !mtx.is_invalid() { let _ = CloseHandle(mtx); }
    }
}
