//! ETW 逐进程流量采集（Microsoft-Windows-Kernel-Network）。
//!
//! 事件 10/16/26/32 是发送，11/17/27/33 是接收；payload 前 8 字节是
//! `u32 PID` + `u32 size`，后面是 `daddr, saddr, dport, sport`（端口和地址都是网络序）。
//! 拿不到会话时（例如没有管理员权限）`start()` 返回 false，上层自动退回网卡总量。
//!
//! 本机到本机的流量（回环、TUN 内部转发、本机网卡自己访问自己）在收发两端各有
//! 一条事件，直接相加会把同一份字节算两遍、并且把“下载”记成某个进程的“上传”
//! （代理转发就是这样）。这里用两层过滤去掉重复：
//! 1. 两端都是本机地址时，只保留“客户端那一份”——进程自己占用的端口不是监听
//!    端口的就是客户端；服务端那份跳过。
//! 2. 两端都是本机地址、又都不是监听端口（tun2socks 之类的内部转发）时整条丢弃。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Diagnostics::Etw::*;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use crate::gfx::log;

pub const SESSION_NAME: &str = "FlowDockNet";

/// Microsoft-Windows-Kernel-Network
const PROVIDER: GUID = GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);

#[derive(Clone, Copy, Default, Debug)]
pub struct Bytes {
    pub up: u64,
    pub down: u64,
}

static TOTALS: Mutex<Option<HashMap<u32, Bytes>>> = Mutex::new(None);
static NAMES: Mutex<Option<HashMap<u32, (String, String, std::time::Instant)>>> = Mutex::new(None);
static DUMPED: AtomicU64 = AtomicU64::new(0);
static ID_COUNTS: [AtomicU64; 40] = [const { AtomicU64::new(0) }; 40];

/// 本机 IPv4 地址（网络序），采样线程每 5 秒刷一次。
static LOCAL_V4: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];
static LOCAL_V4_LEN: AtomicU32 = AtomicU32::new(0);

/// TCP 监听端口位图（主机序），采样线程每 5 秒刷一次。
const LISTENER_WORDS: usize = 1024; // 1024 * 64 bit = 65536 个端口
static LISTENERS: [AtomicU64; LISTENER_WORDS] = [const { AtomicU64::new(0) }; LISTENER_WORDS];

/// 采样线程调用：更新本机地址表。
pub fn set_local_v4(addrs: &[u32]) {
    let n = addrs.len().min(LOCAL_V4.len());
    for (i, a) in addrs.iter().take(n).enumerate() {
        LOCAL_V4[i].store(*a, Ordering::Relaxed);
    }
    for i in n..LOCAL_V4.len() {
        LOCAL_V4[i].store(0, Ordering::Relaxed);
    }
    LOCAL_V4_LEN.store(n as u32, Ordering::Relaxed);
}

/// 采样线程调用：更新 TCP 监听端口表。
pub fn set_listeners(ports: &[u16]) {
    for w in LISTENERS.iter() {
        w.store(0, Ordering::Relaxed);
    }
    for p in ports {
        let w = (*p / 64) as usize;
        let b = (*p % 64) as u32;
        LISTENERS[w].fetch_or(1u64 << b, Ordering::Relaxed);
    }
}

fn is_listener(port: u16) -> bool {
    LISTENERS[(port / 64) as usize].load(Ordering::Relaxed) & (1u64 << (port % 64)) != 0
}

fn is_local_v4(a: u32) -> bool {
    if a == 0 {
        return false;
    }
    if (a & 0xFF00_0000) == 0x7F00_0000 {
        return true; // 127.0.0.0/8，网络序
    }
    let n = LOCAL_V4_LEN.load(Ordering::Relaxed) as usize;
    for i in 0..n {
        if LOCAL_V4[i].load(Ordering::Relaxed) == a {
            return true;
        }
    }
    false
}

fn is_loopback_v6(a: &[u8]) -> bool {
    a.len() == 16 && a[..15].iter().all(|b| *b == 0) && a[15] == 1
}

/// 诊断：各事件 ID 的命中次数。
pub fn id_counts() -> String {
    use std::sync::atomic::Ordering;
    let mut out = String::new();
    for (id, c) in ID_COUNTS.iter().enumerate() {
        let n = c.load(Ordering::Relaxed);
        if n > 0 {
            out.push_str(&format!("{id}:{n} "));
        }
    }
    out
}

/// 诊断：会话丢了多少事件。
pub fn events_lost() -> u32 {
    unsafe {
        let mut buf = props_buffer(SESSION_NAME, 0);
        let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        let rc = ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(props_name_ptr(&buf)),
            props,
            EVENT_TRACE_CONTROL_QUERY,
        );
        if rc == ERROR_SUCCESS {
            (*props).EventsLost
        } else {
            u32::MAX
        }
    }
}

fn ensure_init() {
    let mut t = TOTALS.lock().unwrap();
    if t.is_none() {
        *t = Some(HashMap::new());
    }
    let mut n = NAMES.lock().unwrap();
    if n.is_none() {
        *n = Some(HashMap::new());
    }
}

/// 取走这一秒累计的字节并清空，避免 PID 表无限增长。
pub fn take() -> HashMap<u32, Bytes> {
    match TOTALS.lock() {
        Ok(mut g) => match g.as_mut() {
            Some(map) => std::mem::take(map),
            None => HashMap::new(),
        },
        Err(_) => HashMap::new(),
    }
}

/// 返回 (进程名, 完整路径)。带 30 秒 TTL，避免 PID 复用时一直显示旧名字。
pub fn process_info(pid: u32) -> (String, String) {
    if let Ok(mut n) = NAMES.lock() {
        if let Some(map) = n.as_mut() {
            if let Some((name, path, at)) = map.get(&pid) {
                if at.elapsed().as_secs() < 30 {
                    return (name.clone(), path.clone());
                }
            }
        }
    }
    let full = unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let mut buf = [0u16; 520];
                let mut len = buf.len() as u32;
                let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
                let _ = CloseHandle(h);
                if ok && len > 0 {
                    String::from_utf16_lossy(&buf[..len as usize])
                } else {
                    String::new()
                }
            }
            Err(_) => String::new(),
        }
    };
    let name = if full.is_empty() {
        format!("PID {pid}")
    } else {
        full.rsplit('\\').next().unwrap_or(&full).to_string()
    };
    if let Ok(mut n) = NAMES.lock() {
        if let Some(map) = n.as_mut() {
            if map.len() > 512 {
                map.clear();
            }
            map.insert(pid, (name.clone(), full.clone(), std::time::Instant::now()));
        }
    }
    (name, full)
}

fn debug_dump() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("FLOWDOCK_ETW_DUMP").is_some())
}

unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    let Some(r) = record.as_ref() else { return };
    let eid = r.EventHeader.EventDescriptor.Id;
    if (eid as usize) < ID_COUNTS.len() {
        ID_COUNTS[eid as usize].fetch_add(1, Ordering::Relaxed);
    }
    // (IPv6?, 发送?)
    let (v6, dir_up) = match eid {
        10 => (false, true),
        11 => (false, false),
        16 => (false, true),
        17 => (false, false),
        26 => (true, true),
        27 => (true, false),
        32 => (true, true),
        33 => (true, false),
        _ => return,
    };
    if r.UserData.is_null() {
        return;
    }
    let p = r.UserData as *const u8;
    let len = r.UserDataLength as usize;
    let need = if v6 { 44 } else { 20 };
    if len < need {
        return;
    }
    let pid = u32::from_le_bytes(std::ptr::read_unaligned(p as *const [u8; 4]));
    let size = u32::from_le_bytes(std::ptr::read_unaligned(p.add(4) as *const [u8; 4]));
    if debug_dump() {
        let n = DUMPED.fetch_add(1, Ordering::Relaxed);
        if n < 400 {
            let hexlen = len.min(48);
            let raw = std::slice::from_raw_parts(p, hexlen);
            let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
            log(&format!("ev id={eid} len={len} pid={pid} size={size} raw={hex}"));
        }
    }
    if pid == 0 || size == 0 {
        return;
    }

    // 地址/端口：网络序（大端）。dport 是目标端口，sport 是源端口。
    let (daddr_local, saddr_local, dport, sport) = if v6 {
        let d = std::slice::from_raw_parts(p.add(8), 16);
        let s = std::slice::from_raw_parts(p.add(24), 16);
        (
            is_loopback_v6(d),
            is_loopback_v6(s),
            u16::from_be_bytes(std::ptr::read_unaligned(p.add(40) as *const [u8; 2])),
            u16::from_be_bytes(std::ptr::read_unaligned(p.add(42) as *const [u8; 2])),
        )
    } else {
        (
            is_local_v4(u32::from_be_bytes(std::ptr::read_unaligned(p.add(8) as *const [u8; 4]))),
            is_local_v4(u32::from_be_bytes(std::ptr::read_unaligned(p.add(12) as *const [u8; 4]))),
            u16::from_be_bytes(std::ptr::read_unaligned(p.add(16) as *const [u8; 2])),
            u16::from_be_bytes(std::ptr::read_unaligned(p.add(18) as *const [u8; 2])),
        )
    };

    if daddr_local && saddr_local {
        // 本机到本机：这条连接的两端各有一次事件，只保留客户端那一份。
        let own_port = if dir_up { sport } else { dport };
        let peer_port = if dir_up { dport } else { sport };
        if is_listener(own_port) {
            return; // 服务端那份，客户端那边已经计过了
        }
        if !is_listener(peer_port) {
            return; // 两端都不是监听端口：tun2socks 之类的内部转发，不算
        }
    }

    if let Ok(mut guard) = TOTALS.lock() {
        if let Some(map) = guard.as_mut() {
            let e = map.entry(pid).or_default();
            if dir_up {
                e.up += size as u64;
            } else {
                e.down += size as u64;
            }
        }
    }
}

/// 组装 EVENT_TRACE_PROPERTIES + 会话名的一块连续内存。
fn props_buffer(name: &str, mode: u32) -> Vec<u8> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let struct_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let total = struct_size + wide.len() * 2;
    let mut buf = vec![0u8; total];
    unsafe {
        let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        (*props).Wnode.BufferSize = total as u32;
        (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*props).Wnode.ClientContext = 1; // QPC
        (*props).LogFileMode = mode;
        (*props).FlushTimer = 1;
        (*props).LoggerNameOffset = struct_size as u32;
        std::ptr::copy_nonoverlapping(wide.as_ptr(), buf.as_mut_ptr().add(struct_size) as *mut u16, wide.len());
    }
    buf
}

pub fn stop() {
    stop_existing();
}

fn stop_existing() {
    unsafe {
        let mut buf = props_buffer(SESSION_NAME, 0);
        let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        let _ = ControlTraceW(CONTROLTRACE_HANDLE::default(), PCWSTR(props_name_ptr(&buf)), props, EVENT_TRACE_CONTROL_STOP);
    }
}

fn props_name_ptr(buf: &[u8]) -> *const u16 {
    unsafe {
        let props = buf.as_ptr() as *const EVENT_TRACE_PROPERTIES;
        buf.as_ptr().add((*props).LoggerNameOffset as usize) as *const u16
    }
}

/// 启动实时会话。成功返回 true。
pub fn start() -> bool {
    ensure_init();
    stop_existing();

    let mut handle = CONTROLTRACE_HANDLE::default();
    let name_wide: Vec<u16> = SESSION_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = props_buffer(SESSION_NAME, EVENT_TRACE_REAL_TIME_MODE);
    let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;

    let rc: WIN32_ERROR = unsafe { StartTraceW(&mut handle, PCWSTR(name_wide.as_ptr()), props) };
    if rc != ERROR_SUCCESS {
        log(&format!("ETW StartTrace failed: {}", rc.0));
        return false;
    }

    let rc = unsafe {
        EnableTraceEx2(
            handle,
            &PROVIDER,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            TRACE_LEVEL_INFORMATION as u8,
            0,
            0,
            0,
            None,
        )
    };
    if rc != ERROR_SUCCESS {
        log(&format!("ETW EnableTrace failed: {}", rc.0));
        stop_existing();
        return false;
    }

    std::thread::spawn(move || {
        let name_wide: Vec<u16> = SESSION_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let mut lf: EVENT_TRACE_LOGFILEW = std::mem::zeroed();
            lf.LoggerName = PWSTR(name_wide.as_ptr() as *mut u16);
            lf.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
            lf.Anonymous2.EventRecordCallback = Some(on_event);
            let th = OpenTraceW(&mut lf);
            if th.Value == u64::MAX {
                log("ETW OpenTrace failed");
                return;
            }
            let _ = ProcessTrace(&[th], None, None);
            let _ = CloseTrace(th);
        }
    });
    true
}
