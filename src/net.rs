//! 流量采集：
//! - 卡片总量（up/down）用物理网卡计数器，代理 / TUN / 回环都不会把方向算错；
//! - 进程榜用 ETW（Microsoft-Windows-Kernel-Network），本机内部的重复事件在
//!   `etw::on_event` 里按“本机地址 + 监听端口”去掉；
//! - 没有管理员权限、ETW 起不来时，卡片照样有网卡总量，只是没有进程榜。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetExtendedTcpTable, GetIfTable2, GetIpAddrTable, MIB_IF_TABLE2,
    MIB_IPADDRTABLE, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};

use crate::etw;
use crate::gfx::log;

#[derive(Clone, Debug)]
pub struct ProcSample {
    pub name: String,
    pub up: f64,
    pub down: f64,
    pub path: String,
}

pub struct Sampler {
    /// ETW 各进程之和（面板表头）
    up: AtomicU64,
    down: AtomicU64,
    /// 物理网卡真实速率（卡片）
    nic_up: AtomicU64,
    nic_down: AtomicU64,
    procs: Mutex<Vec<ProcSample>>,
    /// ETW 起不来时，面板要提示需要管理员权限
    pub limited: bool,
}

impl Sampler {
    pub fn start() -> Arc<Sampler> {
        // FLOWDOCK_DEMO=1：截图/演示用，不启动 ETW，返回固定的假数据
        let demo = std::env::var_os("FLOWDOCK_DEMO").is_some();
        let etw_ok = !demo && etw::start();
        log(&format!("ETW session: {etw_ok} demo: {demo}"));

        let s = Arc::new(Sampler {
            up: AtomicU64::new(0),
            down: AtomicU64::new(0),
            nic_up: AtomicU64::new(0),
            nic_down: AtomicU64::new(0),
            procs: Mutex::new(Vec::new()),
            limited: !etw_ok && !demo,
        });

        let c = s.clone();
        thread::spawn(move || {
            // 名字 -> (上行, 下行, 最后一次出现的时间片)
            let mut recent: HashMap<String, (f64, f64, u64)> = HashMap::new();
            let mut paths: HashMap<String, String> = HashMap::new();
            let mut tick: u64 = 0;
            // 卡片总量永远取物理网卡计数器：代理/TUN/回环都不会把方向算错。
            let mut prev_totals = totals();
            if !demo {
                refresh_net_tables();
            }

            loop {
                thread::sleep(Duration::from_millis(1000));
                tick += 1;

                if demo {
                    let rows = demo_rows();
                    let up: f64 = rows.iter().map(|r| r.up).sum();
                    let down: f64 = rows.iter().map(|r| r.down).sum();
                    c.up.store(up as u64, Ordering::Relaxed);
                    c.down.store(down as u64, Ordering::Relaxed);
                    for p in &rows {
                        let _ = crate::icon::get(&p.path);
                    }
                    if let Ok(mut p) = c.procs.lock() {
                        *p = rows;
                    }
                    continue;
                }

                // 本机地址 / 监听端口每 5 秒刷一次，ETW 回调靠它们去重
                if tick % 5 == 1 {
                    refresh_net_tables();
                }
                let cur = totals();
                let nic_up = cur.1.saturating_sub(prev_totals.1);
                let nic_down = cur.0.saturating_sub(prev_totals.0);
                c.nic_up.store(nic_up, Ordering::Relaxed);
                c.nic_down.store(nic_down, Ordering::Relaxed);
                prev_totals = cur;

                if !etw_ok {
                    c.up.store(nic_up, Ordering::Relaxed);
                    c.down.store(nic_down, Ordering::Relaxed);
                    continue;
                }

                let mut fresh: HashMap<String, (f64, f64)> = HashMap::new();
                let mut total_up = 0.0f64;
                let mut total_down = 0.0f64;
                for (pid, b) in etw::take() {
                    let du = b.up as f64;
                    let dd = b.down as f64;
                    if du == 0.0 && dd == 0.0 {
                        continue;
                    }
                    let (name, path) = etw::process_info(pid);
                    let e = fresh.entry(name.clone()).or_insert((0.0, 0.0));
                    e.0 += du;
                    e.1 += dd;
                    total_up += du;
                    total_down += dd;
                    paths.entry(name).or_insert(path);
                }

                // 面板表头 = 各进程之和；卡片用物理网卡真实速率。
                c.up.store(total_up as u64, Ordering::Relaxed);
                c.down.store(total_down as u64, Ordering::Relaxed);

                for (name, (u, d)) in fresh {
                    recent.insert(name, (u, d, tick));
                }
                recent.retain(|_, (_, _, t)| tick.saturating_sub(*t) <= 5);
                paths.retain(|name, _| recent.contains_key(name));

                let mut list: Vec<ProcSample> = recent
                    .iter()
                    .map(|(name, (u, d, t))| {
                        let live = tick.saturating_sub(*t) <= 1;
                        ProcSample {
                            name: name.clone(),
                            up: if live { *u } else { 0.0 },
                            down: if live { *d } else { 0.0 },
                            path: paths.get(name).cloned().unwrap_or_default(),
                        }
                    })
                    .collect();
                list.sort_by(|a, b| {
                    (b.up + b.down).partial_cmp(&(a.up + a.down)).unwrap_or(std::cmp::Ordering::Equal)
                });
                list.truncate(8);

                if std::env::var_os("FLOWDOCK_ETW_DEBUG").is_some() {
                    let su: f64 = list.iter().map(|p| p.up).sum();
                    let sd: f64 = list.iter().map(|p| p.down).sum();
                    log(&format!(
                        "tick {tick}: sum_up={:.1} sum_down={:.1} kB/s | nic_up={:.1} nic_down={:.1} | rows={} | ids={} lost={}",
                        total_up / 1024.0,
                        total_down / 1024.0,
                        nic_up as f64 / 1024.0,
                        nic_down as f64 / 1024.0,
                        list.len(),
                        etw::id_counts(),
                        etw::events_lost()
                    ));
                    let _ = (su, sd);
                }

                // 在采样线程里预热图标，避免首次展开面板时在 UI 线程做 Shell 查询
                for p in &list {
                    let _ = crate::icon::get(&p.path);
                }

                if let Ok(mut p) = c.procs.lock() {
                    *p = list;
                }
            }
        });
        s
    }

    /// 面板表头：各进程的 ETW 归属之和。
    pub fn totals(&self) -> (f64, f64) {
        (
            self.up.load(Ordering::Relaxed) as f64,
            self.down.load(Ordering::Relaxed) as f64,
        )
    }

    /// 卡片：物理网卡计数器差值。
    pub fn nic_totals(&self) -> (f64, f64) {
        (
            self.nic_up.load(Ordering::Relaxed) as f64,
            self.nic_down.load(Ordering::Relaxed) as f64,
        )
    }

    pub fn procs(&self) -> Vec<ProcSample> {
        self.procs.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_OPER_STATUS_UP: i32 = 1;
const AF_INET: u32 = 2;

/// 刷新本机 IPv4 地址表和 TCP 监听端口表，供 ETW 回调识别本机内部流量。
fn refresh_net_tables() {
    let mut addrs: Vec<u32> = Vec::new();
    unsafe {
        let mut size = 0u32;
        let _ = GetIpAddrTable(None, &mut size, false);
        if size >= std::mem::size_of::<MIB_IPADDRTABLE>() as u32 {
            let mut buf = vec![0u8; size as usize];
            let table = buf.as_mut_ptr() as *mut MIB_IPADDRTABLE;
            if GetIpAddrTable(Some(table), &mut size, false) == 0 {
                let n = (*table).dwNumEntries as usize;
                let rows = (*table).table.as_ptr();
                for i in 0..n {
                    addrs.push((*rows.add(i)).dwAddr);
                }
            }
        }
    }
    etw::set_local_v4(&addrs);

    let mut ports: Vec<u16> = Vec::new();
    unsafe {
        let mut size = 0u32;
        let _ = GetExtendedTcpTable(None, &mut size, false, AF_INET, TCP_TABLE_OWNER_PID_LISTENER, 0);
        if size >= std::mem::size_of::<MIB_TCPTABLE_OWNER_PID>() as u32 {
            let mut buf = vec![0u8; size as usize];
            let table = buf.as_mut_ptr() as *mut MIB_TCPTABLE_OWNER_PID;
            let rc = GetExtendedTcpTable(
                Some(table as *mut core::ffi::c_void),
                &mut size,
                false,
                AF_INET,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            );
            if rc == 0 {
                let n = (*table).dwNumEntries as usize;
                let rows = (*table).table.as_ptr();
                for i in 0..n {
                    let raw = ((*rows.add(i)).dwLocalPort & 0xFFFF) as u16;
                    ports.push(u16::from_be(raw));
                }
            }
        }
    }
    etw::set_listeners(&ports);
}

fn totals() -> (u64, u64) {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table) != NO_ERROR || table.is_null() {
            return (0, 0);
        }
        let count = (*table).NumEntries as usize;
        let rows = (*table).Table.as_ptr();
        let mut in_octets: u64 = 0;
        let mut out_octets: u64 = 0;
        for i in 0..count {
            let row = &*rows.add(i);
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK {
                continue;
            }
            // 只统计物理网卡，避免 VPN / 虚拟网卡把同一份流量重复相加
            if row.InterfaceAndOperStatusFlags._bitfield & 1 == 0 {
                continue;
            }
            if row.OperStatus.0 != IF_OPER_STATUS_UP {
                continue;
            }
            in_octets = in_octets.saturating_add(row.InOctets);
            out_octets = out_octets.saturating_add(row.OutOctets);
        }
        FreeMibTable(table as *const core::ffi::c_void);
        (in_octets, out_octets)
    }
}

/// 演示数据：只用 Windows 自带进程，避免把真实在用的软件名带进截图。
pub fn demo_rows() -> Vec<ProcSample> {
    let kb = 1024.0;
    vec![
        ProcSample {
            name: "svchost.exe".into(),
            up: 86.4 * kb,
            down: 3.4 * 1024.0 * kb,
            path: r"C:\Windows\System32\svchost.exe".into(),
        },
        ProcSample {
            name: "powershell.exe".into(),
            up: 24.1 * kb,
            down: 12.0 * kb,
            path: r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe".into(),
        },
        ProcSample {
            name: "explorer.exe".into(),
            up: 1.2 * kb,
            down: 128.6 * kb,
            path: r"C:\Windows\explorer.exe".into(),
        },
    ]
}