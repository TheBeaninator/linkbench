//! RDMA (RoCE / InfiniBand) data plane over N reliable-connected QPs,
//! using SEND/RECV with slotted registered buffer regions. Multiple QPs
//! (driven by one thread each) matter especially for Soft-RoCE, where a
//! single QP is CPU-bound — the analogue of TCP's parallel streams.
//! libibverbs is dlopen'd at runtime — see `ffi.rs`.

pub mod ffi;

use crate::proto::{RdmaDeviceInfo, RdmaEndpoint};
use crate::transport::{merge_timings, DataPlane, RecvTiming};
use anyhow::{anyhow, bail, Result};
use ffi::*;
use std::ffi::CStr;
use std::time::Instant;

const MAX_SLOTS: usize = 256;
const SQ_DEPTH: u32 = 512;
const RQ_DEPTH: u32 = 512;
const CQ_DEPTH: i32 = 1024;
const INLINE_MAX: u32 = 220;

fn mtu_to_enum_bytes(e: u32) -> u32 {
    256 << e.saturating_sub(1).min(4)
}

fn os_err(what: &str) -> anyhow::Error {
    anyhow!("{what}: {}", std::io::Error::last_os_error())
}

fn rc_err(what: &str, rc: i32) -> anyhow::Error {
    anyhow!("{what}: {}", std::io::Error::from_raw_os_error(rc.abs()))
}

/// Same share rule on both ends: message i goes over QP (i mod pattern);
/// counts must pair up exactly between the two sides.
fn shares(count: u64, n: usize) -> Vec<u64> {
    let n = n as u64;
    let base = count / n;
    (0..n).map(|i| base + u64::from(i < count % n)).collect()
}

// ------------------------------------------------------------ GID selection

fn parse_sysfs_gid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.trim().chars().filter(|c| *c != ':').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn is_ipv4_mapped(gid: &[u8; 16]) -> bool {
    gid[..10].iter().all(|b| *b == 0) && gid[10] == 0xff && gid[11] == 0xff
}

fn gid_to_string(gid: &[u8; 16]) -> String {
    if is_ipv4_mapped(gid) {
        format!("::ffff:{}.{}.{}.{}", gid[12], gid[13], gid[14], gid[15])
    } else {
        gid.chunks(2)
            .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// Pick a GID index for RoCE: prefer RoCE v2 with an IPv4-mapped address,
/// then any non-zero RoCE v2 GID, then index 0.
fn choose_gid_index(device: &str, port: u8) -> (i32, [u8; 16]) {
    let base = format!("/sys/class/infiniband/{device}/ports/{port}");
    let mut best: Option<(i32, [u8; 16], i32)> = None; // (idx, gid, score)
    for idx in 0..16 {
        let Ok(gid_s) = std::fs::read_to_string(format!("{base}/gids/{idx}")) else {
            break;
        };
        let Some(gid) = parse_sysfs_gid(&gid_s) else { continue };
        if gid.iter().all(|b| *b == 0) {
            continue;
        }
        let ty = std::fs::read_to_string(format!("{base}/gid_attrs/types/{idx}"))
            .unwrap_or_default();
        let v2 = ty.trim().eq_ignore_ascii_case("roce v2");
        let score = match (v2, is_ipv4_mapped(&gid)) {
            (true, true) => 3,
            (true, false) => 2,
            (false, _) => 1,
        };
        if best.map_or(true, |(_, _, s)| score > s) {
            best = Some((idx, gid, score));
        }
    }
    best.map(|(i, g, _)| (i, g)).unwrap_or((0, [0u8; 16]))
}

// --------------------------------------------------------------- discovery

/// The network interface backing an RDMA device: rxe exposes `parent`,
/// hardware devices expose `device/net/<ifname>`.
pub fn device_netdev(device: &str) -> Option<String> {
    let base = format!("/sys/class/infiniband/{device}");
    if let Ok(parent) = std::fs::read_to_string(format!("{base}/parent")) {
        let p = parent.trim();
        if !p.is_empty() {
            return Some(p.to_string());
        }
    }
    std::fs::read_dir(format!("{base}/device/net"))
        .ok()?
        .flatten()
        .next()
        .map(|e| e.file_name().to_string_lossy().into_owned())
}

pub fn list_devices() -> Result<Vec<RdmaDeviceInfo>> {
    let verbs = Verbs::load()?;
    let mut out = Vec::new();
    unsafe {
        let mut n = 0i32;
        let list = (verbs.ibv_get_device_list)(&mut n);
        if list.is_null() {
            return Ok(out);
        }
        for i in 0..n as isize {
            let dev = *list.offset(i);
            let name = CStr::from_ptr((verbs.ibv_get_device_name)(dev))
                .to_string_lossy()
                .into_owned();
            let ctx = (verbs.ibv_open_device)(dev);
            if ctx.is_null() {
                continue;
            }
            let mut pa: ibv_port_attr = std::mem::zeroed();
            if (verbs.ibv_query_port)(ctx, 1, &mut pa) == 0 {
                out.push(RdmaDeviceInfo {
                    netdev: device_netdev(&name).unwrap_or_default(),
                    name,
                    port_state: match pa.state {
                        4 => "ACTIVE".into(),
                        s => format!("state{s}"),
                    },
                    link_layer: if pa.link_layer == IBV_LINK_LAYER_ETHERNET {
                        "Ethernet(RoCE)".into()
                    } else {
                        "InfiniBand".into()
                    },
                    active_mtu: mtu_to_enum_bytes(pa.active_mtu),
                    rate_note: format!("width x{} speed {}", pa.active_width, pa.active_speed),
                });
            }
            (verbs.ibv_close_device)(ctx);
        }
        (verbs.ibv_free_device_list)(list);
    }
    Ok(out)
}

// ------------------------------------------------------------------ QP unit

/// One QP with its CQs and buffer-region slices. Each unit is driven by at
/// most one thread at a time.
struct QpUnit {
    ctx: *mut ibv_context,
    qp: *mut ibv_qp,
    send_cq: *mut ibv_cq,
    recv_cq: *mut ibv_cq,
    tx_buf: *mut u8,
    rx_buf: *mut u8,
    region: usize,
    tx_lkey: u32,
    rx_lkey: u32,
    max_inline: u32,
}

unsafe impl Send for QpUnit {}
unsafe impl Sync for QpUnit {}

impl QpUnit {
    fn slots_for(&self, size: usize) -> usize {
        (self.region / size.max(1)).clamp(1, MAX_SLOTS)
    }

    unsafe fn post_send_slot(&self, slot: usize, size: usize) -> Result<()> {
        let mut sge = ibv_sge {
            addr: self.tx_buf as u64 + (slot * size) as u64,
            length: size as u32,
            lkey: self.tx_lkey,
        };
        let mut wr = ibv_send_wr::default();
        wr.wr_id = slot as u64;
        wr.sg_list = &mut sge;
        wr.num_sge = 1;
        wr.opcode = IBV_WR_SEND;
        wr.send_flags = IBV_SEND_SIGNALED;
        if size as u32 <= self.max_inline {
            wr.send_flags |= IBV_SEND_INLINE;
        }
        let mut bad: *mut ibv_send_wr = std::ptr::null_mut();
        let rc = ((*(*self.qp).context).ops.post_send)(self.qp, &mut wr, &mut bad);
        if rc != 0 {
            return Err(rc_err("ibv_post_send", rc));
        }
        Ok(())
    }

    unsafe fn post_recv_slot(&self, slot: usize, size: usize) -> Result<()> {
        let mut sge = ibv_sge {
            addr: self.rx_buf as u64 + (slot * size) as u64,
            length: size.max(1) as u32,
            lkey: self.rx_lkey,
        };
        let mut wr = ibv_recv_wr {
            wr_id: slot as u64,
            next: std::ptr::null_mut(),
            sg_list: &mut sge,
            num_sge: 1,
        };
        let mut bad: *mut ibv_recv_wr = std::ptr::null_mut();
        let rc = ((*(*self.qp).context).ops.post_recv)(self.qp, &mut wr, &mut bad);
        if rc != 0 {
            return Err(rc_err("ibv_post_recv", rc));
        }
        Ok(())
    }

    unsafe fn poll(&self, cq: *mut ibv_cq, wcs: &mut [ibv_wc]) -> Result<usize> {
        let n = ((*self.ctx).ops.poll_cq)(cq, wcs.len() as i32, wcs.as_mut_ptr());
        if n < 0 {
            bail!("ibv_poll_cq failed");
        }
        for wc in &wcs[..n as usize] {
            if wc.status != IBV_WC_SUCCESS {
                bail!(
                    "work completion error: status {} vendor_err {} (wr_id {})",
                    wc.status,
                    wc.vendor_err,
                    wc.wr_id
                );
            }
        }
        Ok(n as usize)
    }

    fn send_burst(&self, size: usize, count: u64) -> Result<()> {
        let slots = self.slots_for(size);
        let window = slots.min(SQ_DEPTH as usize - 16) as u64;
        let mut posted: u64 = 0;
        let mut done: u64 = 0;
        let mut wcs = [ibv_wc::default(); 64];
        unsafe {
            while done < count {
                while posted < count && posted - done < window {
                    self.post_send_slot((posted % slots as u64) as usize, size)?;
                    posted += 1;
                }
                done += self.poll(self.send_cq, &mut wcs)? as u64;
            }
        }
        Ok(())
    }

    fn recv_burst(&self, size: usize, count: u64) -> Result<Option<(Instant, Instant)>> {
        let slots = self.slots_for(size);
        let window = slots.min(RQ_DEPTH as usize - 16) as u64;
        let mut posted: u64 = 0;
        let mut done: u64 = 0;
        let mut first: Option<Instant> = None;
        let mut last: Option<Instant> = None;
        let mut wcs = [ibv_wc::default(); 64];
        unsafe {
            while posted < count.min(window) {
                self.post_recv_slot((posted % slots as u64) as usize, size)?;
                posted += 1;
            }
            while done < count {
                let n = self.poll(self.recv_cq, &mut wcs)? as u64;
                if n > 0 {
                    let now = Instant::now();
                    first.get_or_insert(now);
                    last = Some(now);
                    done += n;
                    while posted < count && posted - done < window {
                        self.post_recv_slot((posted % slots as u64) as usize, size)?;
                        posted += 1;
                    }
                }
            }
        }
        Ok(first.zip(last))
    }

    fn bidir_burst(&self, size: usize, count: u64) -> Result<Option<(Instant, Instant)>> {
        let slots = self.slots_for(size);
        let send_window = slots.min(SQ_DEPTH as usize - 16) as u64;
        let recv_window = slots.min(RQ_DEPTH as usize - 16) as u64;
        let (mut s_posted, mut s_done) = (0u64, 0u64);
        let (mut r_posted, mut r_done) = (0u64, 0u64);
        let mut first: Option<Instant> = None;
        let mut last: Option<Instant> = None;
        let mut wcs = [ibv_wc::default(); 64];
        unsafe {
            while r_posted < count.min(recv_window) {
                self.post_recv_slot((r_posted % slots as u64) as usize, size)?;
                r_posted += 1;
            }
            while s_done < count || r_done < count {
                while s_posted < count && s_posted - s_done < send_window {
                    self.post_send_slot((s_posted % slots as u64) as usize, size)?;
                    s_posted += 1;
                }
                if s_done < count {
                    s_done += self.poll(self.send_cq, &mut wcs)? as u64;
                }
                if r_done < count {
                    let n = self.poll(self.recv_cq, &mut wcs)? as u64;
                    if n > 0 {
                        let now = Instant::now();
                        first.get_or_insert(now);
                        last = Some(now);
                        r_done += n;
                        while r_posted < count && r_posted - r_done < recv_window {
                            self.post_recv_slot((r_posted % slots as u64) as usize, size)?;
                            r_posted += 1;
                        }
                    }
                }
            }
        }
        Ok(first.zip(last))
    }

    fn recv_timeline(
        &self,
        size: usize,
        count: u64,
        t0: Instant,
        bucket: std::time::Duration,
    ) -> Result<(Option<(Instant, Instant)>, Vec<u64>)> {
        let slots = self.slots_for(size);
        let window = slots.min(RQ_DEPTH as usize - 16) as u64;
        let mut posted: u64 = 0;
        let mut done: u64 = 0;
        let mut first: Option<Instant> = None;
        let mut last: Option<Instant> = None;
        let mut buckets: Vec<u64> = Vec::new();
        let mut wcs = [ibv_wc::default(); 64];
        unsafe {
            while posted < count.min(window) {
                self.post_recv_slot((posted % slots as u64) as usize, size)?;
                posted += 1;
            }
            while done < count {
                let n = self.poll(self.recv_cq, &mut wcs)? as u64;
                if n > 0 {
                    let now = Instant::now();
                    first.get_or_insert(now);
                    last = Some(now);
                    done += n;
                    let idx =
                        (now.duration_since(t0).as_nanos() / bucket.as_nanos().max(1)) as usize;
                    if idx >= buckets.len() {
                        buckets.resize(idx + 1, 0);
                    }
                    buckets[idx] += n * size as u64;
                    while posted < count && posted - done < window {
                        self.post_recv_slot((posted % slots as u64) as usize, size)?;
                        posted += 1;
                    }
                }
            }
        }
        Ok((first.zip(last), buckets))
    }

    /// Ping with the first payload byte set to `last` (loaded-ping protocol).
    fn ping_flagged(&self, size: usize, last: bool) -> Result<()> {
        unsafe {
            *self.tx_buf = u8::from(last);
        }
        self.ping(size)
    }

    /// One unechoed message to consume the echoer's final posted recv.
    fn send_drain(&self, size: usize) -> Result<()> {
        let mut wcs = [ibv_wc::default(); 16];
        unsafe {
            *self.tx_buf = 0;
            self.post_send_slot(0, size)?;
            while self.poll(self.send_cq, &mut wcs)? == 0 {}
        }
        Ok(())
    }

    /// Echo flagged pings until one arrives with flag byte 1, then consume
    /// the drain message. Recv accounting stays exactly balanced.
    fn echo_flagged(&self, size: usize) -> Result<()> {
        let mut wcs = [ibv_wc::default(); 16];
        unsafe {
            self.post_recv_slot(0, size)?;
            self.post_recv_slot(1, size)?;
            loop {
                let mut got: Option<u64> = None;
                while got.is_none() {
                    if self.poll(self.recv_cq, &mut wcs)? > 0 {
                        got = Some(wcs[0].wr_id);
                    }
                }
                let slot = got.unwrap() as usize;
                let flag = *self.rx_buf.add(slot * size);
                if flag == 1 {
                    // Reply to the last ping; the other posted recv absorbs
                    // the drain message.
                    self.post_send_slot(0, size)?;
                    while self.poll(self.send_cq, &mut wcs)? == 0 {}
                    while self.poll(self.recv_cq, &mut wcs)? == 0 {}
                    return Ok(());
                }
                self.post_recv_slot(slot, size)?;
                self.post_send_slot(0, size)?;
                while self.poll(self.send_cq, &mut wcs)? == 0 {}
            }
        }
    }

    fn ping(&self, size: usize) -> Result<()> {
        let mut wcs = [ibv_wc::default(); 16];
        unsafe {
            self.post_recv_slot(0, size)?;
            self.post_send_slot(0, size)?;
            let mut got_reply = false;
            let mut send_reaped = false;
            while !got_reply || !send_reaped {
                if !send_reaped && self.poll(self.send_cq, &mut wcs)? > 0 {
                    send_reaped = true;
                }
                if !got_reply && self.poll(self.recv_cq, &mut wcs)? > 0 {
                    got_reply = true;
                }
            }
        }
        Ok(())
    }

    fn echo(&self, size: usize, iters: u32) -> Result<()> {
        let mut wcs = [ibv_wc::default(); 16];
        unsafe {
            // Two rotating receive slots: the next recv is always posted
            // before the reply goes out, so the initiator can never hit RNR.
            // Total recvs posted must equal `iters` — a leftover posted recv
            // of this size would corrupt the next test (REM_OP_ERR when a
            // larger message lands in it).
            let pre = 2.min(iters);
            for s in 0..pre {
                self.post_recv_slot(s as usize, size)?;
            }
            let mut posted = pre;
            for _ in 0..iters {
                while self.poll(self.recv_cq, &mut wcs)? == 0 {}
                if posted < iters {
                    self.post_recv_slot((posted % 2) as usize, size)?;
                    posted += 1;
                }
                self.post_send_slot(0, size)?;
                let mut reaped = 0;
                while reaped == 0 {
                    reaped = self.poll(self.send_cq, &mut wcs)?;
                }
            }
        }
        Ok(())
    }
}

// -------------------------------------------------------------------- plane

pub struct RdmaPlane {
    verbs: Verbs,
    ctx: *mut ibv_context,
    pd: *mut ibv_pd,
    tx_mr: *mut ibv_mr,
    rx_mr: *mut ibv_mr,
    tx_buf: *mut u8,
    rx_buf: *mut u8,
    region_total: usize,
    units: Vec<QpUnit>,
    local_ep: RdmaEndpoint,
    port: u8,
    // Cross-round receive-rotation state for reduce_begin/reduce_round.
    red_posted: u32,
    red_rounds: u32,
    red_cur: u64,
}

// Raw pointers are to driver state; each QpUnit is used by one thread at a
// time and the plane itself moves between threads whole.
unsafe impl Send for RdmaPlane {}

impl RdmaPlane {
    pub fn new(
        device: Option<&str>,
        gid_index: Option<i32>,
        region_total: usize,
        nqps: usize,
    ) -> Result<Self> {
        let nqps = nqps.clamp(1, 64);
        let verbs = Verbs::load()?;
        let port: u8 = 1;
        unsafe {
            let mut n = 0i32;
            let list = (verbs.ibv_get_device_list)(&mut n);
            if list.is_null() || n == 0 {
                bail!("no RDMA devices found");
            }
            let mut chosen: *mut ibv_device = std::ptr::null_mut();
            let mut chosen_name = String::new();
            for i in 0..n as isize {
                let dev = *list.offset(i);
                let name = CStr::from_ptr((verbs.ibv_get_device_name)(dev))
                    .to_string_lossy()
                    .into_owned();
                if device.map_or(i == 0, |want| want == name) {
                    chosen = dev;
                    chosen_name = name;
                    if device.is_some() {
                        break;
                    }
                }
            }
            if chosen.is_null() {
                let names: Vec<String> = (0..n as isize)
                    .map(|i| {
                        CStr::from_ptr((verbs.ibv_get_device_name)(*list.offset(i)))
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
                (verbs.ibv_free_device_list)(list);
                bail!(
                    "RDMA device {:?} not found; available: {}",
                    device.unwrap_or(""),
                    names.join(", ")
                );
            }
            let ctx = (verbs.ibv_open_device)(chosen);
            (verbs.ibv_free_device_list)(list);
            if ctx.is_null() {
                return Err(os_err("ibv_open_device"));
            }

            let mut pa: ibv_port_attr = std::mem::zeroed();
            let rc = (verbs.ibv_query_port)(ctx, port, &mut pa);
            if rc != 0 {
                return Err(rc_err("ibv_query_port", rc));
            }

            let (gidx, gid) = match gid_index {
                Some(i) => {
                    let mut g = ibv_gid { raw: [0; 16] };
                    let rc = (verbs.ibv_query_gid)(ctx, port, i, &mut g);
                    if rc != 0 {
                        return Err(rc_err("ibv_query_gid", rc));
                    }
                    (i, g.raw)
                }
                None => choose_gid_index(&chosen_name, port),
            };

            let pd = (verbs.ibv_alloc_pd)(ctx);
            if pd.is_null() {
                return Err(os_err("ibv_alloc_pd"));
            }

            // Whole regions registered once; each QP gets a page-aligned slice.
            let per_qp = (region_total / nqps) & !4095;
            if per_qp == 0 {
                bail!("region too small for {nqps} QPs");
            }
            let region_total = per_qp * nqps;
            let layout = std::alloc::Layout::from_size_align(region_total, 4096).unwrap();
            let tx_buf = std::alloc::alloc_zeroed(layout);
            let rx_buf = std::alloc::alloc_zeroed(layout);
            if tx_buf.is_null() || rx_buf.is_null() {
                bail!("failed to allocate {region_total} byte buffer regions");
            }

            let access = IBV_ACCESS_LOCAL_WRITE;
            let tx_mr = (verbs.ibv_reg_mr)(pd, tx_buf.cast(), region_total, access);
            if tx_mr.is_null() {
                return Err(os_err("ibv_reg_mr(tx) — check `ulimit -l` (locked memory)"));
            }
            let rx_mr = (verbs.ibv_reg_mr)(pd, rx_buf.cast(), region_total, access);
            if rx_mr.is_null() {
                return Err(os_err("ibv_reg_mr(rx) — check `ulimit -l` (locked memory)"));
            }

            let mut units = Vec::with_capacity(nqps);
            let mut qpns = Vec::with_capacity(nqps);
            for qi in 0..nqps {
                let send_cq = (verbs.ibv_create_cq)(
                    ctx,
                    CQ_DEPTH,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                );
                let recv_cq = (verbs.ibv_create_cq)(
                    ctx,
                    CQ_DEPTH,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                );
                if send_cq.is_null() || recv_cq.is_null() {
                    return Err(os_err("ibv_create_cq"));
                }
                let mut init = ibv_qp_init_attr {
                    qp_context: std::ptr::null_mut(),
                    send_cq,
                    recv_cq,
                    srq: std::ptr::null_mut(),
                    cap: ibv_qp_cap {
                        max_send_wr: SQ_DEPTH,
                        max_recv_wr: RQ_DEPTH,
                        max_send_sge: 1,
                        max_recv_sge: 1,
                        max_inline_data: INLINE_MAX,
                    },
                    qp_type: IBV_QPT_RC,
                    sq_sig_all: 0,
                };
                let mut qp = (verbs.ibv_create_qp)(pd, &mut init);
                if qp.is_null() {
                    // Retry without inline in case the device rejects it.
                    init.cap.max_inline_data = 0;
                    qp = (verbs.ibv_create_qp)(pd, &mut init);
                }
                if qp.is_null() {
                    return Err(os_err("ibv_create_qp"));
                }

                // RESET -> INIT
                let mut attr = ibv_qp_attr::default();
                attr.qp_state = IBV_QPS_INIT;
                attr.pkey_index = 0;
                attr.port_num = port;
                attr.qp_access_flags =
                    (IBV_ACCESS_LOCAL_WRITE | IBV_ACCESS_REMOTE_WRITE) as u32;
                let rc = (verbs.ibv_modify_qp)(
                    qp,
                    &mut attr,
                    IBV_QP_STATE | IBV_QP_PKEY_INDEX | IBV_QP_PORT | IBV_QP_ACCESS_FLAGS,
                );
                if rc != 0 {
                    return Err(rc_err("ibv_modify_qp(INIT)", rc));
                }

                qpns.push((*qp).qp_num);
                units.push(QpUnit {
                    ctx,
                    qp,
                    send_cq,
                    recv_cq,
                    tx_buf: tx_buf.add(qi * per_qp),
                    rx_buf: rx_buf.add(qi * per_qp),
                    region: per_qp,
                    tx_lkey: (*tx_mr).lkey,
                    rx_lkey: (*rx_mr).lkey,
                    max_inline: init.cap.max_inline_data,
                });
            }

            let local_ep = RdmaEndpoint {
                qpns,
                psn: 0,
                gid,
                lid: pa.lid,
                mtu: pa.active_mtu,
                device: chosen_name,
                gid_index: gidx,
            };

            Ok(Self {
                verbs,
                ctx,
                pd,
                tx_mr,
                rx_mr,
                tx_buf,
                rx_buf,
                region_total,
                units,
                local_ep,
                port,
                red_posted: 0,
                red_rounds: 0,
                red_cur: 0,
            })
        }
    }

    pub fn local_endpoint(&self) -> RdmaEndpoint {
        self.local_ep.clone()
    }

    /// Bring all QPs to RTS against the peer's endpoint (QP i ↔ QP i).
    pub fn connect(&mut self, remote: RdmaEndpoint) -> Result<()> {
        if remote.qpns.len() != self.units.len() {
            bail!(
                "QP count mismatch: local {} vs remote {}",
                self.units.len(),
                remote.qpns.len()
            );
        }
        for (unit, &remote_qpn) in self.units.iter().zip(&remote.qpns) {
            unsafe {
                // INIT -> RTR
                let mut attr = ibv_qp_attr::default();
                attr.qp_state = IBV_QPS_RTR;
                attr.path_mtu = self.local_ep.mtu.min(remote.mtu).clamp(1, 5);
                attr.dest_qp_num = remote_qpn;
                attr.rq_psn = remote.psn;
                attr.max_dest_rd_atomic = 1;
                attr.min_rnr_timer = 1; // fastest backoff; RNR only during warm-up
                attr.ah_attr.port_num = self.port;
                attr.ah_attr.dlid = remote.lid;
                if remote.lid == 0 {
                    // RoCE: address by GID
                    attr.ah_attr.is_global = 1;
                    attr.ah_attr.grh.dgid = ibv_gid { raw: remote.gid };
                    attr.ah_attr.grh.sgid_index = self.local_ep.gid_index as u8;
                    attr.ah_attr.grh.hop_limit = 64;
                }
                let rc = (self.verbs.ibv_modify_qp)(
                    unit.qp,
                    &mut attr,
                    IBV_QP_STATE
                        | IBV_QP_AV
                        | IBV_QP_PATH_MTU
                        | IBV_QP_DEST_QPN
                        | IBV_QP_RQ_PSN
                        | IBV_QP_MAX_DEST_RD_ATOMIC
                        | IBV_QP_MIN_RNR_TIMER,
                );
                if rc != 0 {
                    return Err(rc_err("ibv_modify_qp(RTR)", rc));
                }

                // RTR -> RTS
                let mut attr = ibv_qp_attr::default();
                attr.qp_state = IBV_QPS_RTS;
                attr.sq_psn = self.local_ep.psn;
                attr.timeout = 14;
                attr.retry_cnt = 7;
                attr.rnr_retry = 7; // infinite — receiver posting lag is expected
                attr.max_rd_atomic = 1;
                let rc = (self.verbs.ibv_modify_qp)(
                    unit.qp,
                    &mut attr,
                    IBV_QP_STATE
                        | IBV_QP_TIMEOUT
                        | IBV_QP_RETRY_CNT
                        | IBV_QP_RNR_RETRY
                        | IBV_QP_SQ_PSN
                        | IBV_QP_MAX_QP_RD_ATOMIC,
                );
                if rc != 0 {
                    return Err(rc_err("ibv_modify_qp(RTS)", rc));
                }
            }
        }
        Ok(())
    }

    /// Fan a burst out over the QP units on scoped threads.
    fn fan_out<F, R>(&self, count: u64, f: F) -> Result<Vec<R>>
    where
        F: Fn(&QpUnit, u64) -> Result<R> + Sync,
        R: Send,
    {
        let parts = shares(count, self.units.len());
        if parts.iter().filter(|c| **c > 0).count() <= 1 {
            // Single active QP: no threads needed.
            return Ok(vec![f(&self.units[0], count)?]);
        }
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for (unit, share) in self.units.iter().zip(parts) {
                if share == 0 {
                    continue;
                }
                let f = &f;
                handles.push(scope.spawn(move || f(unit, share)));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })
    }
}

impl Drop for RdmaPlane {
    fn drop(&mut self) {
        unsafe {
            for u in &self.units {
                (self.verbs.ibv_destroy_qp)(u.qp);
                (self.verbs.ibv_destroy_cq)(u.send_cq);
                (self.verbs.ibv_destroy_cq)(u.recv_cq);
            }
            (self.verbs.ibv_dereg_mr)(self.tx_mr);
            (self.verbs.ibv_dereg_mr)(self.rx_mr);
            (self.verbs.ibv_dealloc_pd)(self.pd);
            (self.verbs.ibv_close_device)(self.ctx);
            let layout = std::alloc::Layout::from_size_align(self.region_total, 4096).unwrap();
            std::alloc::dealloc(self.tx_buf, layout);
            std::alloc::dealloc(self.rx_buf, layout);
        }
    }
}

impl DataPlane for RdmaPlane {
    fn kind(&self) -> &'static str {
        "rdma"
    }

    fn max_msg(&self) -> usize {
        self.units[0].region
    }

    fn describe(&self) -> String {
        format!(
            "device {} (on {}) gid[{}] {} mtu {} inline {}B, {} QP",
            self.local_ep.device,
            device_netdev(&self.local_ep.device).unwrap_or_else(|| "?".into()),
            self.local_ep.gid_index,
            gid_to_string(&self.local_ep.gid),
            mtu_to_enum_bytes(self.local_ep.mtu),
            self.units[0].max_inline,
            self.units.len(),
        )
    }

    fn send_burst(&mut self, size: usize, count: u64) -> Result<()> {
        if size > self.max_msg() {
            bail!("message size {size} exceeds per-QP region {}", self.max_msg());
        }
        self.fan_out(count, |u, share| u.send_burst(size, share))?;
        Ok(())
    }

    fn recv_burst(&mut self, size: usize, count: u64) -> Result<RecvTiming> {
        if size > self.max_msg() {
            bail!("message size {size} exceeds per-QP region {}", self.max_msg());
        }
        let parts = self.fan_out(count, |u, share| u.recv_burst(size, share))?;
        Ok(merge_timings(parts, count))
    }

    fn bidir_burst(&mut self, size: usize, count: u64) -> Result<RecvTiming> {
        if size > self.max_msg() {
            bail!("message size {size} exceeds per-QP region {}", self.max_msg());
        }
        let parts = self.fan_out(count, |u, share| u.bidir_burst(size, share))?;
        Ok(merge_timings(parts, count))
    }

    fn ping(&mut self, size: usize) -> Result<()> {
        self.units[0].ping(size)
    }

    fn echo(&mut self, size: usize, iters: u32) -> Result<()> {
        self.units[0].echo(size, iters)
    }

    fn lanes(&self) -> usize {
        self.units.len()
    }

    fn reduce_begin(&mut self, chunk: usize, rounds: u32) -> Result<()> {
        let pre = 2.min(rounds);
        unsafe {
            for s in 0..pre {
                self.units[0].post_recv_slot(s as usize, chunk)?;
            }
        }
        self.red_posted = pre;
        self.red_rounds = rounds;
        self.red_cur = 0;
        Ok(())
    }

    /// Two rotating receive slots, exactly like `echo`: the recv for round
    /// i+1 is posted before round i completes, so the peer can never hit
    /// RNR backoff mid-measurement; total recvs posted equals `rounds`.
    fn reduce_round(&mut self, chunk: usize) -> Result<()> {
        let slot = (self.red_cur % 2) as usize;
        let post_more = self.red_posted < self.red_rounds;
        {
            let u = &self.units[0];
            let mut wcs = [ibv_wc::default(); 16];
            unsafe {
                u.post_send_slot(0, chunk)?;
                let mut send_done = false;
                let mut recv_done = false;
                while !send_done || !recv_done {
                    if !send_done && u.poll(u.send_cq, &mut wcs)? > 0 {
                        send_done = true;
                    }
                    if !recv_done && u.poll(u.recv_cq, &mut wcs)? > 0 {
                        recv_done = true;
                    }
                }
                if post_more {
                    u.post_recv_slot(slot, chunk)?;
                }
            }
        }
        if post_more {
            self.red_posted += 1;
        }
        self.red_cur += 1;
        Ok(())
    }

    fn recv_timeline(
        &mut self,
        size: usize,
        count: u64,
        bucket: std::time::Duration,
    ) -> Result<(RecvTiming, Vec<u64>, usize)> {
        if size > self.max_msg() {
            bail!("message size {size} exceeds per-QP region {}", self.max_msg());
        }
        let t0 = Instant::now();
        let parts = self.fan_out(count, |u, share| u.recv_timeline(size, share, t0, bucket))?;
        let mut all = Vec::new();
        let mut timings = Vec::new();
        for (t, b) in parts {
            timings.push(t);
            if b.len() > all.len() {
                all.resize(b.len(), 0);
            }
            for (i, v) in b.into_iter().enumerate() {
                all[i] += v;
            }
        }
        let steady = crate::transport::steady_buckets(&timings, t0, bucket, all.len());
        Ok((merge_timings(timings, count), all, steady))
    }

    fn loaded_ping_initiator(
        &mut self,
        ping_size: usize,
        bulk_size: usize,
        bulk_count: u64,
    ) -> Result<Vec<u64>> {
        let (u0, bulk_units) = self.units.split_first().expect("at least one QP");
        if bulk_units.is_empty() {
            bail!("loaded ping needs at least 2 QPs");
        }
        let n = bulk_units.len() as u64;
        let remaining = std::sync::atomic::AtomicUsize::new(bulk_units.len());
        std::thread::scope(|scope| -> Result<Vec<u64>> {
            for (i, unit) in bulk_units.iter().enumerate() {
                let share = bulk_count / n + u64::from((i as u64) < bulk_count % n);
                let remaining = &remaining;
                scope.spawn(move || {
                    let r = unit.send_burst(bulk_size, share);
                    remaining.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    r
                });
            }
            let mut rtts = Vec::new();
            loop {
                let last = remaining.load(std::sync::atomic::Ordering::SeqCst) == 0;
                let t = Instant::now();
                u0.ping_flagged(ping_size, last)?;
                rtts.push(t.elapsed().as_nanos() as u64);
                if last {
                    break;
                }
            }
            u0.send_drain(ping_size)?;
            Ok(rtts)
        })
    }

    fn loaded_ping_echoer(
        &mut self,
        ping_size: usize,
        bulk_size: usize,
        bulk_count: u64,
    ) -> Result<()> {
        let (u0, bulk_units) = self.units.split_first().expect("at least one QP");
        if bulk_units.is_empty() {
            bail!("loaded ping needs at least 2 QPs");
        }
        let n = bulk_units.len() as u64;
        std::thread::scope(|scope| -> Result<()> {
            let mut handles = Vec::new();
            for (i, unit) in bulk_units.iter().enumerate() {
                let share = bulk_count / n + u64::from((i as u64) < bulk_count % n);
                handles.push(scope.spawn(move || unit.recv_burst(bulk_size, share)));
            }
            u0.echo_flagged(ping_size)?;
            for h in handles {
                h.join().unwrap()?;
            }
            Ok(())
        })
    }
}
