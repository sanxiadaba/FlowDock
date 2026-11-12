//! 流量采集：有 ETW 时按进程累计并求和；没有 ETW 时退回网卡计数器。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};

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
    up: AtomicU64,
    down: AtomicU64,
    procs: Mutex<Vec<ProcSample>>,
    pub etw_active: bool,
}

impl Sampler {
    pub fn start() -> Arc<Sampler> {
        let etw_ok = etw::start();
        log(&format!("ETW session: {etw_ok}"));

        let s = Arc::new(Sampler {
            up: AtomicU64::new(0),
            down: AtomicU64::new(0),
            procs: Mutex::new(Vec::new()),
            etw_active: etw_ok,
        });

        let c = s.clone();
        thread::spawn(move || {
            // 名字 -> (上行, 下行, 最后一次出现的时间片)
            let mut recent: HashMap<String, (f64, f64, u64)> = HashMap::new();
            let mut paths: HashMap<String, String> = HashMap::new();
            let mut tick: u64 = 0;
            let mut prev_totals = if etw_ok { (0, 0) } else { totals() };

            loop {
                thread::sleep(Duration::from_millis(1000));
                tick += 1;

                if !etw_ok {
                    let cur = totals();
                    let d_in = cur.0.saturating_sub(prev_totals.0) as f64;
                    let d_out = cur.1.saturating_sub(prev_totals.1) as f64;
                    c.up.store(d_out as u64, Ordering::Relaxed);
                    c.down.store(d_in as u64, Ordering::Relaxed);
                    prev_totals = cur;
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

                // 卡片总量 = 各进程之和，和下面的榜单永远对得上
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
                        "tick {tick}: sum_up={:.1} sum_down={:.1} kB/s | rows={} | ids={} lost={}",
                        total_up / 1024.0,
                        total_down / 1024.0,
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

    pub fn totals(&self) -> (f64, f64) {
        (
            self.up.load(Ordering::Relaxed) as f64,
            self.down.load(Ordering::Relaxed) as f64,
        )
    }

    pub fn procs(&self) -> Vec<ProcSample> {
        self.procs.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_OPER_STATUS_UP: i32 = 1;

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
