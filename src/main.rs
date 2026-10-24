// CPS Boost: tiny background click-rate booster for Windows (left + right button).
// Idle cost: main thread blocks in GetMessage and worker threads are parked => ~0% CPU.
//
// Hotkeys:  Ctrl+Alt+B = pause/resume     Ctrl+Alt+Q = quit
#![windows_subsystem = "windows"] // no console window

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---- Settings (edit these, then rebuild) ----
const TRIGGER_CPS: usize = 6; // real CPS needed to activate
const MIN_TARGET_CPS: u64 = 14; // boosted rate range (set both equal for a fixed rate)
const MAX_TARGET_CPS: u64 = 20;
const STOP_AFTER_IDLE_MS: u64 = 300; // stop boosting this long after you release the button
const BUTTON_ENABLED: [bool; 2] = [true, true]; // [left, right]

const FLAGS: [(u32, u32); 2] = [
    (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
];

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
                if BUTTON_ENABLED[i] {
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
            let rate = MIN_TARGET_CPS + rand() % (MAX_TARGET_CPS - MIN_TARGET_CPS + 1);
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
    thread::spawn(|| popup("CPS Boost is running (left + right click).\nCtrl+Alt+B = pause/resume\nCtrl+Alt+Q = quit"));
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
