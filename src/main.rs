// CPS Boost: tiny background click-rate booster for Windows (left + right button).
// A small settings window appears at start: pick buttons + clicks per second, press Start.
// Idle cost: main thread blocks in GetMessage and worker threads are parked => ~0% CPU.
//
// Hotkeys:  Ctrl+Alt+B = pause/resume     Ctrl+Alt+Q = quit
#![windows_subsystem = "windows"] // no console window

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT};
use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// Fixed Windows values defined here so no extra library features are needed.
const BST_CHECKED: isize = 1;
const COLOR_WINDOW_IDX: isize = 5;

const TRIGGER_CPS: usize = 4; // real clicks per second needed to activate
const STOP_AFTER_IDLE_MS: u64 = 300; // stop boosting this long after you release the button

const FLAGS: [(u32, u32); 2] = [
    (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
];

// ---- Settings chosen in the start window ----
static TARGET_CPS: AtomicU64 = AtomicU64::new(20);
static BUTTON_ENABLED: [AtomicBool; 2] = [AtomicBool::new(true), AtomicBool::new(true)]; // [left, right]

struct Btn {
    down: AtomicBool,
    boosting: AtomicBool,
    last_ms: AtomicU64,
    clicks: Mutex<VecDeque<Instant>>,
    worker: OnceLock<Thread>,
}

impl Btn {
    const fn new() -> Self {
        Btn {
            down: AtomicBool::new(false),
            boosting: AtomicBool::new(false),
            last_ms: AtomicU64::new(0),
            clicks: Mutex::new(VecDeque::new()),
            worker: OnceLock::new(),
        }
    }
}

static BTNS: [Btn; 2] = [Btn::new(), Btn::new()]; // 0 = left, 1 = right
static ENABLED: AtomicBool = AtomicBool::new(true);
static START: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    START.get().map(|s| s.elapsed().as_millis() as u64).unwrap_or(0)
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: usize, lparam: isize) -> isize {
    if code >= 0 && ENABLED.load(Relaxed) {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        // Ignore clicks we injected ourselves (prevents a feedback loop).
        if info.flags & LLMHF_INJECTED == 0 {
            let hit = match wparam as u32 {
                WM_LBUTTONDOWN => Some((0usize, true)),
                WM_LBUTTONUP => Some((0, false)),
                WM_RBUTTONDOWN => Some((1, true)),
                WM_RBUTTONUP => Some((1, false)),
                _ => None,
            };
            if let Some((i, is_down)) = hit {
                if BUTTON_ENABLED[i].load(Relaxed) {
                    let b = &BTNS[i];
                    if is_down {
                        b.down.store(true, Relaxed);
                        on_real_click(i);
                    } else {
                        b.down.store(false, Relaxed);
                    }
                    // While boosting, hide real clicks so the game sees one clean stream.
                    if b.boosting.load(Relaxed) {
                        return 1;
                    }
                }
            }
        }
    }
    CallNextHookEx(0, code, wparam, lparam)
}

fn on_real_click(i: usize) {
    let b = &BTNS[i];
    let now = Instant::now();
    b.last_ms.store(now_ms(), Relaxed);
    let count = {
        let mut q = match b.clicks.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        q.push_back(now);
        while let Some(&front) = q.front() {
            if now.duration_since(front) > Duration::from_secs(1) {
                q.pop_front();
            } else {
                break;
            }
        }
        q.len()
    };
    if count >= TRIGGER_CPS && !b.boosting.swap(true, Relaxed) {
        if let Some(w) = b.worker.get() {
            w.unpark();
        }
    }
}

fn send_mouse(flag: u32) {
    unsafe {
        let mut input: INPUT = std::mem::zeroed();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi = MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: 0,
            dwFlags: flag,
            time: 0,
            dwExtraInfo: 0,
        };
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

fn worker(i: usize) {
    let b = &BTNS[i];
    let (down_flag, up_flag) = FLAGS[i];
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15 ^ ((i as u64 + 1) << 32) ^ (Instant::now().elapsed().as_nanos() as u64);
    let mut rand = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    loop {
        if !b.boosting.load(Relaxed) {
            thread::park(); // fully asleep while idle
            continue;
        }
        unsafe { timeBeginPeriod(1) }; // precise sleeps, only while boosting
        loop {
            let idle = now_ms().saturating_sub(b.last_ms.load(Relaxed));
            if !ENABLED.load(Relaxed) || (!b.down.load(Relaxed) && idle > STOP_AFTER_IDLE_MS) {
                break;
            }
            let rate = TARGET_CPS.load(Relaxed).max(1);
            let hold_us = 1000 + (rand() % 4001); // 1-5 ms button hold
            send_mouse(down_flag);
            thread::sleep(Duration::from_micros(hold_us));
            send_mouse(up_flag);
            let period = 1_000_000 / rate;
            thread::sleep(Duration::from_micros(period.saturating_sub(hold_us)));
        }
        unsafe { timeEndPeriod(1) };
        b.boosting.store(false, Relaxed);
        // Re-check: a click may have arrived just as we were stopping.
        let idle = now_ms().saturating_sub(b.last_ms.load(Relaxed));
        if ENABLED.load(Relaxed) && (b.down.load(Relaxed) || idle <= STOP_AFTER_IDLE_MS) {
            b.boosting.store(true, Relaxed);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn popup(text: &str) {
    let t = wide(text);
    let title = wide("CPS Boost");
    unsafe { MessageBoxW(0, t.as_ptr(), title.as_ptr(), MB_OK | MB_ICONINFORMATION) };
}

// ---- Saved settings (file next to the exe) ----
fn config_path() -> Option<std::path::PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("cps-boost-settings.txt")))
}

fn load_config() -> (bool, bool, u64) {
    let (mut l, mut r, mut cps) = (true, true, 20u64);
    if let Some(text) = config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                match k.trim() {
                    "left" => l = v.trim() == "1",
                    "right" => r = v.trim() == "1",
                    "cps" => cps = v.trim().parse().unwrap_or(20).clamp(1, 100),
                    _ => {}
                }
            }
        }
    }
    (l, r, cps)
}

fn save_config(l: bool, r: bool, cps: u64) {
    if let Some(p) = config_path() {
        let _ = std::fs::write(p, format!("left={}\nright={}\ncps={}\n", l as u8, r as u8, cps));
    }
}

// ---- Start window ----
static STARTED: AtomicBool = AtomicBool::new(false);
static H_CHK: [AtomicIsize; 2] = [AtomicIsize::new(0), AtomicIsize::new(0)];
static H_EDIT: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn dlg_proc(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize {
    match msg {
        WM_COMMAND if (wparam & 0xFFFF) == 100 => {
            let checked: isize = BST_CHECKED;
            let l = SendMessageW(H_CHK[0].load(Relaxed), BM_GETCHECK, 0, 0) == checked;
            let r = SendMessageW(H_CHK[1].load(Relaxed), BM_GETCHECK, 0, 0) == checked;
            let mut buf = [0u16; 16];
            let n = GetWindowTextW(H_EDIT.load(Relaxed), buf.as_mut_ptr(), 16);
            let cps = String::from_utf16_lossy(&buf[..n.max(0) as usize])
                .trim()
                .parse::<u64>()
                .unwrap_or(20)
                .clamp(1, 100);
            if !l && !r {
                popup("Please tick at least one button.");
                return 0;
            }
            BUTTON_ENABLED[0].store(l, Relaxed);
            BUTTON_ENABLED[1].store(r, Relaxed);
            TARGET_CPS.store(cps, Relaxed);
            save_config(l, r, cps);
            STARTED.store(true, Relaxed);
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn make_ctl(
    parent: isize,
    hinst: isize,
    class: &str,
    text: &str,
    style: u32,
    (x, y, w, h): (i32, i32, i32, i32),
    id: isize,
) -> isize {
    let c = wide(class);
    let t = wide(text);
    let hw = CreateWindowExW(
        0,
        c.as_ptr(),
        t.as_ptr(),
        style | WS_CHILD as u32 | WS_VISIBLE as u32,
        x,
        y,
        w,
        h,
        parent,
        id,
        hinst,
        std::ptr::null(),
    );
    SendMessageW(hw, WM_SETFONT, GetStockObject(DEFAULT_GUI_FONT) as usize, 1);
    hw
}

/// Shows the settings window. Returns true if the user pressed Start.
unsafe fn show_start_window() -> bool {
    let hinst = GetModuleHandleW(std::ptr::null());
    let class = wide("CpsBoostStart");
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(dlg_proc);
    wc.hInstance = hinst;
    wc.hCursor = LoadCursorW(0, IDC_ARROW);
    wc.hbrBackground = COLOR_WINDOW_IDX + 1;
    wc.lpszClassName = class.as_ptr();
    RegisterClassW(&wc);

    let title = wide("CPS Boost");
    let hwnd = CreateWindowExW(
        0,
        class.as_ptr(),
        title.as_ptr(),
        WS_OVERLAPPED as u32 | WS_CAPTION as u32 | WS_SYSMENU as u32 | WS_VISIBLE as u32,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        300,
        250,
        0,
        0,
        hinst,
        std::ptr::null(),
    );
    if hwnd == 0 {
        return false;
    }
    let (l, r, cps) = load_config();
    make_ctl(hwnd, hinst, "STATIC", "Choose which buttons to boost:", 0, (15, 12, 260, 20), 0);
    let hl = make_ctl(hwnd, hinst, "BUTTON", "Left click", BS_AUTOCHECKBOX as u32 | WS_TABSTOP as u32, (30, 38, 200, 22), 1);
    let hr = make_ctl(hwnd, hinst, "BUTTON", "Right click", BS_AUTOCHECKBOX as u32 | WS_TABSTOP as u32, (30, 62, 200, 22), 2);
    make_ctl(hwnd, hinst, "STATIC", "Clicks per second (1-100):", 0, (15, 100, 170, 20), 0);
    let he = make_ctl(hwnd, hinst, "EDIT", &cps.to_string(), WS_BORDER as u32 | ES_NUMBER as u32 | ES_AUTOHSCROLL as u32 | WS_TABSTOP as u32, (190, 97, 60, 22), 3);
    make_ctl(hwnd, hinst, "STATIC", "Pause: Ctrl+Alt+B   Quit: Ctrl+Alt+Q", 0, (15, 132, 260, 20), 0);
    make_ctl(hwnd, hinst, "BUTTON", "Start", BS_DEFPUSHBUTTON as u32 | WS_TABSTOP as u32, (90, 165, 110, 30), 100);
    SendMessageW(hl, BM_SETCHECK, l as usize, 0);
    SendMessageW(hr, BM_SETCHECK, r as usize, 0);
    H_CHK[0].store(hl, Relaxed);
    H_CHK[1].store(hr, Relaxed);
    H_EDIT.store(he, Relaxed);

    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, 0, 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    STARTED.load(Relaxed)
}

fn main() {
    // Single-instance guard: a second copy just tells you and exits.
    unsafe {
        let name = wide("Local\\CpsBoostSingleInstance");
        let m = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if GetLastError() == 183 {
            popup("CPS Boost is already running.\nPress Ctrl+Alt+Q to quit it.");
            return;
        }
        std::mem::forget(m); // keep the mutex for the life of the program
    }

    if !unsafe { show_start_window() } {
        return; // window closed without pressing Start
    }

    let _ = START.set(Instant::now());
    for i in 0..2 {
        let h = thread::spawn(move || worker(i));
        let _ = BTNS[i].worker.set(h.thread().clone());
    }

    unsafe {
        let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), 0, 0);
        if hook == 0 {
            return;
        }
        RegisterHotKey(0, 1, MOD_CONTROL | MOD_ALT, 'B' as u32);
        RegisterHotKey(0, 2, MOD_CONTROL | MOD_ALT, 'Q' as u32);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            if msg.message == WM_HOTKEY {
                match msg.wParam {
                    1 => {
                        let v = ENABLED.load(Relaxed);
                        ENABLED.store(!v, Relaxed);
                    }
                    2 => break,
                    _ => {}
                }
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(hook);
    }
}
