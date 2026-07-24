//! Minimal hand-rolled libibverbs FFI, loaded at runtime via dlopen so the
//! binary has no hard dependency on rdma-core. Struct layouts mirror
//! rdma-core v50 `verbs.h`; the ABI of these public structs is stable.
//!
//! Only the fields we touch are typed; everything else in shared structs is
//! opaque padding. Structs the driver allocates (`ibv_context`, `ibv_qp`,
//! `ibv_cq`, ...) are only ever accessed through fields that precede any
//! pthread members, so their layout tails don't matter.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

// ---------------------------------------------------------------- constants

pub const IBV_QPT_RC: c_uint = 2;

pub const IBV_QPS_INIT: c_uint = 1;
pub const IBV_QPS_RTR: c_uint = 2;
pub const IBV_QPS_RTS: c_uint = 3;

pub const IBV_QP_STATE: c_int = 1 << 0;
pub const IBV_QP_ACCESS_FLAGS: c_int = 1 << 3;
pub const IBV_QP_PKEY_INDEX: c_int = 1 << 4;
pub const IBV_QP_PORT: c_int = 1 << 5;
pub const IBV_QP_AV: c_int = 1 << 7;
pub const IBV_QP_PATH_MTU: c_int = 1 << 8;
pub const IBV_QP_TIMEOUT: c_int = 1 << 9;
pub const IBV_QP_RETRY_CNT: c_int = 1 << 10;
pub const IBV_QP_RNR_RETRY: c_int = 1 << 11;
pub const IBV_QP_RQ_PSN: c_int = 1 << 12;
pub const IBV_QP_MAX_QP_RD_ATOMIC: c_int = 1 << 13;
pub const IBV_QP_MIN_RNR_TIMER: c_int = 1 << 15;
pub const IBV_QP_SQ_PSN: c_int = 1 << 16;
pub const IBV_QP_MAX_DEST_RD_ATOMIC: c_int = 1 << 17;
pub const IBV_QP_DEST_QPN: c_int = 1 << 20;

pub const IBV_ACCESS_LOCAL_WRITE: c_int = 1;
pub const IBV_ACCESS_REMOTE_WRITE: c_int = 1 << 1;

pub const IBV_WR_RDMA_WRITE: c_uint = 0;
pub const IBV_WR_SEND: c_uint = 2;
pub const IBV_SEND_SIGNALED: c_uint = 1 << 1;
pub const IBV_SEND_INLINE: c_uint = 1 << 3;

pub const IBV_WC_SUCCESS: c_uint = 0;

// ------------------------------------------------------------------ structs

#[repr(C)]
pub struct ibv_device {
    _opaque: [u8; 0],
}

/// Function-pointer dispatch table embedded in `ibv_context`. Field order is
/// ABI: `poll_cq`, `post_send`, `post_recv` are inline-only in verbs.h and
/// must be called through this table.
#[repr(C)]
pub struct ibv_context_ops {
    pub _compat_query_device: *mut c_void,
    pub _compat_query_port: *mut c_void,
    pub _compat_alloc_pd: *mut c_void,
    pub _compat_dealloc_pd: *mut c_void,
    pub _compat_reg_mr: *mut c_void,
    pub _compat_rereg_mr: *mut c_void,
    pub _compat_dereg_mr: *mut c_void,
    pub alloc_mw: *mut c_void,
    pub bind_mw: *mut c_void,
    pub dealloc_mw: *mut c_void,
    pub _compat_create_cq: *mut c_void,
    pub poll_cq: unsafe extern "C" fn(*mut ibv_cq, c_int, *mut ibv_wc) -> c_int,
    pub req_notify_cq: *mut c_void,
    pub _compat_cq_event: *mut c_void,
    pub _compat_resize_cq: *mut c_void,
    pub _compat_destroy_cq: *mut c_void,
    pub _compat_create_srq: *mut c_void,
    pub _compat_modify_srq: *mut c_void,
    pub _compat_query_srq: *mut c_void,
    pub _compat_destroy_srq: *mut c_void,
    pub post_srq_recv: *mut c_void,
    pub _compat_create_qp: *mut c_void,
    pub _compat_query_qp: *mut c_void,
    pub _compat_modify_qp: *mut c_void,
    pub _compat_destroy_qp: *mut c_void,
    pub post_send:
        unsafe extern "C" fn(*mut ibv_qp, *mut ibv_send_wr, *mut *mut ibv_send_wr) -> c_int,
    pub post_recv:
        unsafe extern "C" fn(*mut ibv_qp, *mut ibv_recv_wr, *mut *mut ibv_recv_wr) -> c_int,
    // remaining _compat_* entries never accessed
}

#[repr(C)]
pub struct ibv_context {
    pub device: *mut ibv_device,
    pub ops: ibv_context_ops,
    pub cmd_fd: c_int,
    pub async_fd: c_int,
    pub num_comp_vectors: c_int,
    // pthread_mutex_t + abi_compat follow; never accessed from Rust
}

#[repr(C)]
pub struct ibv_pd {
    pub context: *mut ibv_context,
    pub handle: u32,
}

#[repr(C)]
pub struct ibv_mr {
    pub context: *mut ibv_context,
    pub pd: *mut ibv_pd,
    pub addr: *mut c_void,
    pub length: usize,
    pub handle: u32,
    pub lkey: u32,
    pub rkey: u32,
}

#[repr(C)]
pub struct ibv_cq {
    pub context: *mut ibv_context,
    pub channel: *mut c_void,
    pub cq_context: *mut c_void,
    pub handle: u32,
    pub cqe: c_int,
    // pthread members follow; never accessed
}

#[repr(C)]
pub struct ibv_qp {
    pub context: *mut ibv_context,
    pub qp_context: *mut c_void,
    pub pd: *mut ibv_pd,
    pub send_cq: *mut ibv_cq,
    pub recv_cq: *mut ibv_cq,
    pub srq: *mut c_void,
    pub handle: u32,
    pub qp_num: u32,
    pub state: c_uint,
    pub qp_type: c_uint,
    // pthread members follow; never accessed
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union ibv_gid {
    pub raw: [u8; 16],
}

#[repr(C)]
pub struct ibv_global_route {
    pub dgid: ibv_gid,
    pub flow_label: u32,
    pub sgid_index: u8,
    pub hop_limit: u8,
    pub traffic_class: u8,
}

#[repr(C)]
pub struct ibv_ah_attr {
    pub grh: ibv_global_route,
    pub dlid: u16,
    pub sl: u8,
    pub src_path_bits: u8,
    pub static_rate: u8,
    pub is_global: u8,
    pub port_num: u8,
}

#[repr(C)]
pub struct ibv_qp_cap {
    pub max_send_wr: u32,
    pub max_recv_wr: u32,
    pub max_send_sge: u32,
    pub max_recv_sge: u32,
    pub max_inline_data: u32,
}

#[repr(C)]
pub struct ibv_qp_init_attr {
    pub qp_context: *mut c_void,
    pub send_cq: *mut ibv_cq,
    pub recv_cq: *mut ibv_cq,
    pub srq: *mut c_void,
    pub cap: ibv_qp_cap,
    pub qp_type: c_uint,
    pub sq_sig_all: c_int,
}

#[repr(C)]
pub struct ibv_qp_attr {
    pub qp_state: c_uint,
    pub cur_qp_state: c_uint,
    pub path_mtu: c_uint,
    pub path_mig_state: c_uint,
    pub qkey: u32,
    pub rq_psn: u32,
    pub sq_psn: u32,
    pub dest_qp_num: u32,
    pub qp_access_flags: c_uint,
    pub cap: ibv_qp_cap,
    pub ah_attr: ibv_ah_attr,
    pub alt_ah_attr: ibv_ah_attr,
    pub pkey_index: u16,
    pub alt_pkey_index: u16,
    pub en_sqd_async_notify: u8,
    pub sq_draining: u8,
    pub max_rd_atomic: u8,
    pub max_dest_rd_atomic: u8,
    pub min_rnr_timer: u8,
    pub port_num: u8,
    pub timeout: u8,
    pub retry_cnt: u8,
    pub rnr_retry: u8,
    pub alt_port_num: u8,
    pub alt_timeout: u8,
    pub rate_limit: u32,
}

impl Default for ibv_qp_attr {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
pub struct ibv_port_attr {
    pub state: c_uint,
    pub max_mtu: c_uint,
    pub active_mtu: c_uint,
    pub gid_tbl_len: c_int,
    pub port_cap_flags: u32,
    pub max_msg_sz: u32,
    pub bad_pkey_cntr: u32,
    pub qkey_viol_cntr: u32,
    pub pkey_tbl_len: u16,
    pub lid: u16,
    pub sm_lid: u16,
    pub lmc: u8,
    pub max_vl_num: u8,
    pub sm_sl: u8,
    pub subnet_timeout: u8,
    pub init_type_reply: u8,
    pub active_width: u8,
    pub active_speed: u8,
    pub phys_state: u8,
    pub link_layer: u8,
    pub flags: u8,
    pub port_cap_flags2: u16,
    pub active_speed_ex: u32,
}

pub const IBV_LINK_LAYER_ETHERNET: u8 = 2;

#[repr(C)]
pub struct ibv_sge {
    pub addr: u64,
    pub length: u32,
    pub lkey: u32,
}

/// Layout matches C `struct ibv_send_wr` (128 bytes on x86-64). The `wr`
/// union is flattened to its `rdma`/`atomic` view, which covers the full
/// union size; trailing unions we never use are `_tail` padding.
#[repr(C)]
pub struct ibv_send_wr {
    pub wr_id: u64,
    pub next: *mut ibv_send_wr,
    pub sg_list: *mut ibv_sge,
    pub num_sge: c_int,
    pub opcode: c_uint,
    pub send_flags: c_uint,
    pub imm_data: u32,
    pub wr_rdma_remote_addr: u64,
    pub wr_atomic_compare_add: u64,
    pub wr_atomic_swap: u64,
    pub wr_rdma_rkey: u32,
    pub _wr_pad: u32,
    pub qp_type_xrc_remote_srqn: u32,
    pub _pad2: u32,
    pub _tail: [u64; 6],
}

impl Default for ibv_send_wr {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
pub struct ibv_recv_wr {
    pub wr_id: u64,
    pub next: *mut ibv_recv_wr,
    pub sg_list: *mut ibv_sge,
    pub num_sge: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ibv_wc {
    pub wr_id: u64,
    pub status: c_uint,
    pub opcode: c_uint,
    pub vendor_err: u32,
    pub byte_len: u32,
    pub imm_data: u32,
    pub qp_num: u32,
    pub src_qp: u32,
    pub wc_flags: c_uint,
    pub pkey_index: u16,
    pub slid: u16,
    pub sl: u8,
    pub dlid_path_bits: u8,
}

impl Default for ibv_wc {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// ------------------------------------------------------------------ library

macro_rules! verbs_fns {
    ($( $name:ident : fn( $($arg:ty),* ) -> $ret:ty ; )*) => {
        pub struct Verbs {
            _lib: libloading::Library,
            $( pub $name: unsafe extern "C" fn( $($arg),* ) -> $ret, )*
        }

        impl Verbs {
            pub fn load() -> anyhow::Result<Self> {
                let lib = unsafe {
                    libloading::Library::new("libibverbs.so.1")
                        .or_else(|_| libloading::Library::new("libibverbs.so"))
                }.map_err(|e| anyhow::anyhow!(
                    "libibverbs not found ({e}); install package `libibverbs1` (Debian/Ubuntu) or `rdma-core`"))?;
                unsafe {
                    $( let $name = *lib.get::<unsafe extern "C" fn( $($arg),* ) -> $ret>(
                            concat!(stringify!($name), "\0").as_bytes())?; )*
                    Ok(Self { _lib: lib, $( $name ),* })
                }
            }
        }
    };
}

verbs_fns! {
    ibv_get_device_list: fn(*mut c_int) -> *mut *mut ibv_device;
    ibv_free_device_list: fn(*mut *mut ibv_device) -> ();
    ibv_get_device_name: fn(*mut ibv_device) -> *const c_char;
    ibv_open_device: fn(*mut ibv_device) -> *mut ibv_context;
    ibv_close_device: fn(*mut ibv_context) -> c_int;
    ibv_alloc_pd: fn(*mut ibv_context) -> *mut ibv_pd;
    ibv_dealloc_pd: fn(*mut ibv_pd) -> c_int;
    ibv_reg_mr: fn(*mut ibv_pd, *mut c_void, usize, c_int) -> *mut ibv_mr;
    ibv_dereg_mr: fn(*mut ibv_mr) -> c_int;
    ibv_create_cq: fn(*mut ibv_context, c_int, *mut c_void, *mut c_void, c_int) -> *mut ibv_cq;
    ibv_destroy_cq: fn(*mut ibv_cq) -> c_int;
    ibv_create_qp: fn(*mut ibv_pd, *mut ibv_qp_init_attr) -> *mut ibv_qp;
    ibv_destroy_qp: fn(*mut ibv_qp) -> c_int;
    ibv_modify_qp: fn(*mut ibv_qp, *mut ibv_qp_attr, c_int) -> c_int;
    ibv_query_port: fn(*mut ibv_context, u8, *mut ibv_port_attr) -> c_int;
    ibv_query_gid: fn(*mut ibv_context, u8, c_int, *mut ibv_gid) -> c_int;
}
