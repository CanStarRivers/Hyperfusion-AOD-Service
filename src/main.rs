/*
Hyperfusion AOD Wake Service
License: Creative Commons Attribution-NonCommercial 4.0 International (CC BY-NC 4.0)
You may use, share, and adapt this code for personal, educational, or non-commercial purposes only.
Commercial use, selling, or charging is strictly prohibited.
Full license: https://creativecommons.org/licenses/by-nc/4.0/legalcode
*/

use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::mem;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const PREF_PATH: &str =
    "/data/user_de/0/com.miui.aod/shared_prefs/com.miui.aod_preferences.xml";
const BACKLIGHT_NODE: &str = "/sys/class/backlight/panel0-backlight/brightness";
const WAKE_LOCK_NODE: &str = "/sys/power/wake_lock";
const WAKE_UNLOCK_NODE: &str = "/sys/power/wake_unlock";
const SUSPEND_STATE_NODE: &str = "/sys/devices/virtual/touch/touch_dev/suspend_state";
const MY_AOD_WAKE_LOCK: &str = "vendor.hyperfusion.aod.display-service";

const AOD_TIMEOUT: Duration = Duration::from_secs(10);
const TARGET_BRIGHTNESS: i32 = 180;
const FADE_DELAY_US: u64 = 2000;

const EV_KEY: u16 = 1;
const GESTURE_KEY_1: u16 = 354;
const GESTURE_KEY_2: u16 = 338;

const INOTIFY_HDR: usize = 16;
const INOTIFY_BUF: usize = 512;

#[repr(C)]
struct InputEvent {
    sec: i64,
    usec: i64,
    ev_type: u16,
    code: u16,
    value: i32,
}

struct ScreenState {
    on: bool,
    deadline: Option<Instant>,
}

struct AodService {
    is_aod_enabled: AtomicBool,
    screen_mtx: Mutex<ScreenState>,
    timer_cond: Condvar,
}

impl AodService {
    fn new() -> Self {
        Self {
            is_aod_enabled: AtomicBool::new(true),
            screen_mtx: Mutex::new(ScreenState { on: false, deadline: None }),
            timer_cond: Condvar::new(),
        }
    }

    fn trigger_wakeup(&self) {
        if !self.is_aod_enabled.load(Ordering::Relaxed) {
            return;
        }
        if get_suspend_state() == 0 {
            return;
        }
        let mut s = self.screen_mtx.lock().unwrap();
        s.deadline = Some(Instant::now() + AOD_TIMEOUT);
        self.timer_cond.notify_one();
        if !s.on {
            s.on = true;
            write_node(WAKE_LOCK_NODE, MY_AOD_WAKE_LOCK);
            fade_brightness(0, TARGET_BRIGHTNESS); // 持锁渐亮
        }
    }
    
    fn handle_timeout(&self) {
        loop {
            let mut s = self.screen_mtx.lock().unwrap();
            
            s = self
                .timer_cond
                .wait_while(s, |s| s.deadline.is_none())
                .unwrap();
                
            let fired = loop {
                let deadline = match s.deadline {
                    Some(d) => d,
                    None => break false,
                };
                let now = Instant::now();
                if now >= deadline {
                    s.deadline = None;
                    break true;
                }
                let (guard, res) =
                    self.timer_cond.wait_timeout(s, deadline - now).unwrap();
                s = guard;
                if res.timed_out()
                    && s.deadline.map_or(false, |d| Instant::now() >= d)
                {
                    s.deadline = None;
                    break true;
                }
            };

            if fired && s.on {
                if get_suspend_state() == 0 {
                    // 屏幕已被外部关闭，仅释放 wake lock
                    s.on = false;
                    write_node(WAKE_UNLOCK_NODE, MY_AOD_WAKE_LOCK);
                } else {
                    // 持锁渐暗（与 Go 的 timerMutex 作用域一致）
                    fade_brightness(TARGET_BRIGHTNESS, 0);
                    s.on = false;
                    write_node(WAKE_UNLOCK_NODE, MY_AOD_WAKE_LOCK);
                    drop(s); // force_deep_sleep 前释放锁
                    force_deep_sleep();
                }
            }
        }
    }
    
    fn monitor_input(&self, device_path: &str) {
        let mut file = match fs::File::open(device_path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let ev_size = mem::size_of::<InputEvent>();
        let mut buf = vec![0u8; ev_size];
        loop {
            if file.read_exact(&mut buf).is_err() {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            // SAFETY:
            let ev: &InputEvent = unsafe { &*(buf.as_ptr() as *const InputEvent) };
            if ev.ev_type == EV_KEY
                && (ev.code == GESTURE_KEY_1 || ev.code == GESTURE_KEY_2)
                && ev.value == 1
            {
                self.trigger_wakeup();
            }
        }
    }
    
    fn run_config_watcher(&self) {
        watch_pref_loop(|enabled| {
            self.is_aod_enabled.store(enabled, Ordering::Relaxed);
            true
        });
    }
}

fn main() {
    if !path_exists(BACKLIGHT_NODE) || !path_exists(SUSPEND_STATE_NODE) {
        return;
    }

    if !read_aod_enabled() {
        // on_change 返回 false 时循环停止，即 enabled=true 时退出
        watch_pref_loop(|enabled| !enabled);
    }

    let touch_node = match find_fts_touch_node() {
        Some(n) => n,
        None => return,
    };

    let svc = Arc::new(AodService::new());

    {
        let s = svc.clone();
        thread::Builder::new()
            .name("aod-timeout".into())
            .spawn(move || s.handle_timeout())
            .expect("spawn timeout thread");
    }
    {
        let s = svc.clone();
        thread::Builder::new()
            .name("aod-input".into())
            .spawn(move || s.monitor_input(&touch_node))
            .expect("spawn input thread");
    }
    
    svc.run_config_watcher();
}

fn read_aod_enabled() -> bool {
    fs::read_to_string(PREF_PATH).map_or(false, |data| {
        data.lines()
            .any(|l| l.contains("aod_temporary_style") && l.contains("\"true\""))
    })
}

fn watch_pref_loop(mut on_change: impl FnMut(bool) -> bool) {
    let pref_dir = match Path::new(PREF_PATH).parent().and_then(|p| p.to_str()) {
        Some(d) => d,
        None => return poll_pref_loop(on_change),
    };
    let target_file = Path::new(PREF_PATH)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let dir_c = match CString::new(pref_dir) {
        Ok(c) => c,
        Err(_) => return poll_pref_loop(on_change),
    };

    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        return poll_pref_loop(on_change);
    }

    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            dir_c.as_ptr(),
            libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO,
        )
    };
    if wd < 0 {
        unsafe { libc::close(fd) };
        return poll_pref_loop(on_change);
    }

    let mut buf = [0u8; INOTIFY_BUF];

    'outer: loop {
        let n = unsafe {
            libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, INOTIFY_BUF)
        };
        if n <= 0 {
            continue;
        }
        let n = n as usize;
        let mut off = 0usize;

        while off + INOTIFY_HDR <= n {
            // name_len 位于事件头 [off+12 .. off+16]
            let name_len = u32::from_ne_bytes(
                buf[off + 12..off + 16].try_into().unwrap(),
            ) as usize;

            let next_off = off + INOTIFY_HDR + name_len;

            if name_len > 0 && next_off <= n {
                let name_bytes = &buf[off + INOTIFY_HDR..next_off];
                let nul = name_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(name_bytes.len());
                let name = std::str::from_utf8(&name_bytes[..nul]).unwrap_or("");

                if name == target_file && !on_change(read_aod_enabled()) {
                    break 'outer;
                }
            }

            // 防止格式异常的零长事件导致死循环
            off = if next_off > off { next_off } else { off + INOTIFY_HDR };
        }
    }

    unsafe { libc::close(fd) };
}

fn poll_pref_loop(mut on_change: impl FnMut(bool) -> bool) {
    loop {
        if !on_change(read_aod_enabled()) {
            return;
        }
        thread::sleep(Duration::from_secs(5));
    }
}

fn find_fts_touch_node() -> Option<String> {
    for entry in fs::read_dir("/sys/class/input/").ok()?.filter_map(|e| e.ok()) {
        let path = entry.path();
        let fname = path.file_name()?.to_str()?;
        if !fname.starts_with("event") {
            continue;
        }
        if let Ok(data) = fs::read_to_string(path.join("device/name")) {
            if data.to_ascii_lowercase().contains("fts") {
                return Some(format!("/dev/input/{}", fname));
            }
        }
    }
    None
}

fn get_suspend_state() -> i32 {
    fs::read_to_string(SUSPEND_STATE_NODE)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

fn write_node(path: &str, val: &str) {
    let _ = fs::write(path, val.as_bytes());
}

fn fade_brightness(from: i32, to: i32) {
    if from == to {
        return;
    }
    let step: i32 = if from < to { 1 } else { -1 };
    let mut cur = from;
    loop {
        write_node(BACKLIGHT_NODE, &cur.to_string());
        thread::sleep(Duration::from_micros(FADE_DELAY_US));
        if cur == to {
            break;
        }
        cur += step;
    }
}

fn force_deep_sleep() {
    for lock in ["PowerManagerService.noSuspend", "SensorsHAL_WAKEUP"] {
        write_node(WAKE_UNLOCK_NODE, lock);
    }
}
