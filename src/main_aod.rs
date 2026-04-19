use obfstr::obfstr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use libc::{poll as libc_poll, pollfd, POLLERR, POLLPRI};
use ndk_sys::{
    ASensorEvent, ASensorEventQueue_enableSensor, ASensorEventQueue_getEvents,
    ASensorEventQueue_setEventRate, ASensorManager_createEventQueue,
    ASensorManager_getDefaultSensor, ASensorManager_getInstance, ALooper_prepare,
    ASENSOR_TYPE_LIGHT,
};

static LUX_MAP: &[(f32, u32)] = &[
    (1.0,        50),
    (10.0,       200),
    (50.0,      350),
    (200.0,    550),
    (500.0,    1023),
    (1000.0,   2047),
    (3000.0,   3095),
    (6000.0,   4095),
    (f32::MAX, 4095),
];

#[inline(always)]
fn lux_to_brightness(lux: f32) -> u32 {
    for &(threshold, brightness) in LUX_MAP {
        if lux < threshold {
            return brightness;
        }
    }
    LUX_MAP.last().unwrap().1
}

#[inline(always)]
fn sysfs_read(f: &mut File, buf: &mut [u8]) -> usize {
    let _ = f.seek(SeekFrom::Start(0));
    f.read(buf).unwrap_or(0)
}

#[inline(always)]
fn is_dpms_on(buf: &[u8], n: usize) -> bool {
    n >= 2 && buf[0] == b'O' && buf[1] == b'n'
}

#[inline(always)]
fn parse_u32(buf: &[u8], n: usize) -> u32 {
    let mut val = 0u32;
    for &b in &buf[..n] {
        match b {
            b'0'..=b'9' => val = val * 10 + (b - b'0') as u32,
            _ => break,
        }
    }
    val
}

#[inline(always)]
fn sysfs_write_u32(f: &mut File, val: u32) {
    let mut buf = [0u8; 10];
    if val == 0 {
        let _ = f.write(obfstr!("0").as_bytes());
        return;
    }
    let mut tmp = val;
    let mut len = 0usize;
    while tmp > 0 {
        buf[9 - len] = b'0' + (tmp % 10) as u8;
        tmp /= 10;
        len += 1;
    }
    let _ = f.write(&buf[10 - len..]);
}

#[inline(always)]
fn wait_dpms_change(fd: i32) -> bool {
    let mut pfd = pollfd {
        fd,
        events: (POLLPRI | POLLERR) as i16,
        revents: 0,
    };
    let ret = unsafe { libc_poll(&mut pfd, 1, 500) };
    ret > 0
}

fn probe_poll_support(fd: i32) -> bool {
    let mut pfd = pollfd { fd, events: (POLLPRI | POLLERR) as i16, revents: 0 };
    unsafe { libc_poll(&mut pfd, 1, 100) };

    let start = std::time::Instant::now();
    unsafe { libc_poll(&mut pfd, 1, 2) };
    let elapsed = start.elapsed();

    elapsed.as_millis() >= 1
}

fn spawn_sensor_thread(lux_out: Arc<AtomicU32>) {
    thread::Builder::new()
        .name(obfstr!("lux-sensor").into())
        .spawn(move || unsafe {
            let sm = ASensorManager_getInstance();
            if sm.is_null() {
                return;
            }
            let sensor = ASensorManager_getDefaultSensor(sm, ASENSOR_TYPE_LIGHT as i32);
            if sensor.is_null() {
                return;
            }
            let looper = ALooper_prepare(0);
            let queue  = ASensorManager_createEventQueue(
                sm, looper, 0, None, ptr::null_mut(),
            );
            ASensorEventQueue_enableSensor(queue, sensor);
            ASensorEventQueue_setEventRate(queue, sensor, 100_000);
            let mut event: ASensorEvent = std::mem::zeroed();
            loop {
                while ASensorEventQueue_getEvents(queue, &mut event, 1) > 0 {
                    let lux = event.__bindgen_anon_1.__bindgen_anon_1.data[0];
                    lux_out.store(lux.to_bits(), Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(50));
            }
        })
        .expect(obfstr!("failed to spawn sensor thread"));
}

#[inline(never)]
fn do_inject(bright_r: &mut File, bright_w: &mut File,
             bright_buf: &mut [u8; 8], lux_bits: &AtomicU32)
{
    let bn = sysfs_read(bright_r, bright_buf);
    if parse_u32(bright_buf, bn) == 0 {
        let lux    = f32::from_bits(lux_bits.load(Ordering::Relaxed));
        let target = lux_to_brightness(lux);

        sysfs_write_u32(bright_w, target);
        
        thread::sleep(Duration::from_millis(8));
        sysfs_write_u32(bright_w, target);
    }
}

fn main() {

    let lux_bits: Arc<AtomicU32> = Arc::new(AtomicU32::new(200f32.to_bits()));
    spawn_sensor_thread(Arc::clone(&lux_bits));
    thread::sleep(Duration::from_millis(200));

    let mut dpms_f = File::open(obfstr!("/sys/class/drm/card0-DSI-1/dpms"))
        .unwrap_or_else(|e| panic!("{} {}", obfstr!("open dpms:"), e));
    let mut bright_r = File::open(obfstr!("/sys/class/backlight/panel0-backlight/brightness"))
        .unwrap_or_else(|e| panic!("{} {}", obfstr!("open brightness(r):"), e));
    let mut bright_w = OpenOptions::new().write(true).open(obfstr!("/sys/class/backlight/panel0-backlight/brightness"))
        .unwrap_or_else(|e| panic!("{} {}", obfstr!("open brightness(w):"), e));

    let dpms_fd = dpms_f.as_raw_fd();

    let mut dpms_buf:   [u8; 8] = [0; 8];
    let mut bright_buf: [u8; 8] = [0; 8];

    let n = sysfs_read(&mut dpms_f, &mut dpms_buf);
    let mut last_on = is_dpms_on(&dpms_buf, n);

    let use_poll = probe_poll_support(dpms_fd);

    loop {
        if use_poll {
            wait_dpms_change(dpms_fd);
        } else {
            thread::sleep(Duration::from_millis(8));
        }

        let n = sysfs_read(&mut dpms_f, &mut dpms_buf);
        let now_on = is_dpms_on(&dpms_buf, n);

        if !last_on && now_on {
            do_inject(&mut bright_r, &mut bright_w, &mut bright_buf, &lux_bits);
        }

        last_on = now_on;
    }
}
