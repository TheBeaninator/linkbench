//! Data-plane abstraction. Both ends of a test run the same trait; the
//! bench layer choreographs who sends and who receives via the control
//! channel. Message sizes/counts are agreed out-of-band, so no framing
//! bytes travel on the wire — what we measure is payload.

use anyhow::{bail, Context, Result};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::time::Instant;

/// Receive-side timing of a burst: completion time of the first and last
/// message. Rate = (n-1)*size / (last-first), immune to clock skew and
/// to ramp-up on the sending side.
#[derive(Clone, Copy)]
pub struct RecvTiming {
    pub first: Instant,
    pub last: Instant,
    pub count: u64,
}

pub trait DataPlane: Send {
    fn kind(&self) -> &'static str;
    fn max_msg(&self) -> usize;
    /// A short human description of the path (streams, device, mtu...).
    fn describe(&self) -> String;

    fn send_burst(&mut self, size: usize, count: u64) -> Result<()>;
    fn recv_burst(&mut self, size: usize, count: u64) -> Result<RecvTiming>;
    /// Send and receive `count` messages each way concurrently; returns
    /// local receive timing.
    fn bidir_burst(&mut self, size: usize, count: u64) -> Result<RecvTiming>;
    /// Send one message and wait for the same-sized echo.
    fn ping(&mut self, size: usize) -> Result<()>;
    /// Echo `iters` messages of `size`.
    fn echo(&mut self, size: usize, iters: u32) -> Result<()>;

    /// Number of parallel lanes (TCP streams / RDMA QPs). Lane 0 carries
    /// latency traffic; the loaded-ping test needs at least 2.
    fn lanes(&self) -> usize;

    /// Like `recv_burst`, but also bucket received bytes into `bucket`-wide
    /// time windows from the first call instant (sustained-throughput graph).
    /// The second value is the number of leading buckets during which every
    /// lane was still active — the steady window; later buckets are lane
    /// wind-down (static share skew), not link behaviour.
    fn recv_timeline(
        &mut self,
        size: usize,
        count: u64,
        bucket: std::time::Duration,
    ) -> Result<(RecvTiming, Vec<u64>, usize)>;

    /// Ping continuously on lane 0 (RTTs returned, ns) while lanes 1..N
    /// send `bulk_count` messages of `bulk_size`. The final ping carries
    /// flag byte 1, followed by one unechoed drain message.
    fn loaded_ping_initiator(
        &mut self,
        ping_size: usize,
        bulk_size: usize,
        bulk_count: u64,
    ) -> Result<Vec<u64>>;

    /// Counterpart: echo flagged pings on lane 0 while lanes 1..N receive
    /// the bulk stream.
    fn loaded_ping_echoer(
        &mut self,
        ping_size: usize,
        bulk_size: usize,
        bulk_count: u64,
    ) -> Result<()>;

    /// Prepare `rounds` pipelined small-all-reduce rounds of `chunk` bytes:
    /// receives are pre-posted across rounds (real collectives do this), so
    /// a round can never hit receiver-not-ready backoff.
    fn reduce_begin(&mut self, chunk: usize, rounds: u32) -> Result<()>;
    /// One round: exchange `chunk` bytes both directions with the peer.
    fn reduce_round(&mut self, chunk: usize) -> Result<()>;
}

// ------------------------------------------------------------------- TCP

const SOCK_BUF: usize = 16 << 20;

pub struct TcpPlane {
    streams: Vec<TcpStream>,
    tx: Vec<Vec<u8>>,
    rx: Vec<Vec<u8>>,
    max_msg: usize,
}

fn tune(stream: &TcpStream) {
    stream.set_nodelay(true).ok();
    let fd = stream.as_raw_fd();
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        let v: libc::c_int = SOCK_BUF as libc::c_int;
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                &v as *const _ as *const libc::c_void,
                std::mem::size_of_val(&v) as libc::socklen_t,
            );
        }
    }
}

impl TcpPlane {
    pub fn new(streams: Vec<TcpStream>, max_msg: usize) -> Self {
        for s in &streams {
            tune(s);
        }
        let n = streams.len();
        Self {
            streams,
            tx: (0..n).map(|_| vec![0u8; max_msg]).collect(),
            rx: (0..n).map(|_| vec![0u8; max_msg]).collect(),
            max_msg,
        }
    }

    /// Client side: open `n` data connections to `addr`.
    pub fn connect(addr: &str, n: u16, max_msg: usize) -> Result<Self> {
        let mut streams = Vec::new();
        for i in 0..n {
            streams.push(
                TcpStream::connect(addr)
                    .with_context(|| format!("connect data stream {i} to {addr}"))?,
            );
        }
        Ok(Self::new(streams, max_msg))
    }

    /// Server side: accept `n` data connections.
    pub fn accept(listener: &TcpListener, n: u16, max_msg: usize) -> Result<Self> {
        let mut streams = Vec::new();
        for _ in 0..n {
            streams.push(listener.accept().context("accept data stream")?.0);
        }
        Ok(Self::new(streams, max_msg))
    }

    /// Split `count` messages across the streams (stream 0 gets remainder).
    fn shares(&self, count: u64) -> Vec<u64> {
        let n = self.streams.len() as u64;
        let base = count / n;
        (0..n).map(|i| base + u64::from(i < count % n)).collect()
    }

    /// Small bursts stay on stream 0 without spawning threads; the rule is a
    /// pure function of (size, count) so both ends agree on stream layout.
    fn small(&self, size: usize, count: u64) -> bool {
        size as u64 * count <= 1 << 20
    }
}

fn tcp_send(stream: &TcpStream, buf: &[u8], count: u64) -> Result<()> {
    let mut s = stream;
    for _ in 0..count {
        s.write_all(buf).context("data stream write")?;
    }
    Ok(())
}

fn tcp_recv(stream: &TcpStream, buf: &mut [u8], count: u64) -> Result<Option<(Instant, Instant)>> {
    let mut s = stream;
    let mut first = None;
    let mut last = None;
    for _ in 0..count {
        s.read_exact(buf).context("data stream read")?;
        let now = Instant::now();
        first.get_or_insert(now);
        last = Some(now);
    }
    Ok(first.zip(last))
}

/// Buckets fully covered by the earliest-finishing lane: index of the
/// bucket in which the first lane completed its share.
pub fn steady_buckets(
    parts: &[Option<(Instant, Instant)>],
    t0: Instant,
    bucket: std::time::Duration,
    total: usize,
) -> usize {
    parts
        .iter()
        .flatten()
        .map(|(_, last)| (last.duration_since(t0).as_nanos() / bucket.as_nanos().max(1)) as usize)
        .min()
        .unwrap_or(total)
        .min(total)
}

pub fn merge_timings(parts: Vec<Option<(Instant, Instant)>>, count: u64) -> RecvTiming {
    let mut first: Option<Instant> = None;
    let mut last: Option<Instant> = None;
    for (f, l) in parts.into_iter().flatten() {
        first = Some(first.map_or(f, |c| c.min(f)));
        last = Some(last.map_or(l, |c| c.max(l)));
    }
    let now = Instant::now();
    RecvTiming { first: first.unwrap_or(now), last: last.unwrap_or(now), count }
}

impl DataPlane for TcpPlane {
    fn kind(&self) -> &'static str {
        "tcp"
    }

    fn max_msg(&self) -> usize {
        self.max_msg
    }

    fn describe(&self) -> String {
        let peer = self.streams[0]
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        format!("{} parallel stream(s) to {peer}, {}MiB socket buffers", self.streams.len(), SOCK_BUF >> 20)
    }

    fn send_burst(&mut self, size: usize, count: u64) -> Result<()> {
        if size > self.max_msg {
            bail!("message size {size} exceeds buffer {}", self.max_msg);
        }
        if self.small(size, count) {
            return tcp_send(&self.streams[0], &self.tx[0][..size], count);
        }
        let shares = self.shares(count);
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for ((stream, buf), share) in self.streams.iter().zip(&self.tx).zip(shares) {
                handles.push(scope.spawn(move || tcp_send(stream, &buf[..size], share)));
            }
            for h in handles {
                h.join().unwrap()?;
            }
            Ok(())
        })
    }

    fn recv_burst(&mut self, size: usize, count: u64) -> Result<RecvTiming> {
        if size > self.max_msg {
            bail!("message size {size} exceeds buffer {}", self.max_msg);
        }
        if self.small(size, count) {
            let part = tcp_recv(&self.streams[0], &mut self.rx[0][..size], count)?;
            return Ok(merge_timings(vec![part], count));
        }
        let shares = self.shares(count);
        let parts = std::thread::scope(|scope| -> Result<Vec<_>> {
            let mut handles = Vec::new();
            for ((stream, buf), share) in self.streams.iter().zip(&mut self.rx).zip(shares) {
                handles.push(scope.spawn(move || tcp_recv(stream, &mut buf[..size], share)));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })?;
        Ok(merge_timings(parts, count))
    }

    fn bidir_burst(&mut self, size: usize, count: u64) -> Result<RecvTiming> {
        if size > self.max_msg {
            bail!("message size {size} exceeds buffer {}", self.max_msg);
        }
        let shares = self.shares(count);
        let parts = std::thread::scope(|scope| -> Result<Vec<_>> {
            let mut rx_handles = Vec::new();
            let mut tx_handles = Vec::new();
            for (((stream, txb), rxb), share) in self
                .streams
                .iter()
                .zip(&self.tx)
                .zip(&mut self.rx)
                .zip(shares)
            {
                tx_handles.push(scope.spawn(move || tcp_send(stream, &txb[..size], share)));
                rx_handles.push(scope.spawn(move || tcp_recv(stream, &mut rxb[..size], share)));
            }
            for h in tx_handles {
                h.join().unwrap()?;
            }
            rx_handles.into_iter().map(|h| h.join().unwrap()).collect()
        })?;
        Ok(merge_timings(parts, count))
    }

    fn ping(&mut self, size: usize) -> Result<()> {
        let mut s = &self.streams[0];
        s.write_all(&self.tx[0][..size])?;
        s.read_exact(&mut self.rx[0][..size])?;
        Ok(())
    }

    fn echo(&mut self, size: usize, iters: u32) -> Result<()> {
        let mut s = &self.streams[0];
        for _ in 0..iters {
            s.read_exact(&mut self.rx[0][..size])?;
            let reply: &[u8] = &self.rx[0][..size];
            s.write_all(reply)?;
        }
        Ok(())
    }

    fn lanes(&self) -> usize {
        self.streams.len()
    }

    fn recv_timeline(
        &mut self,
        size: usize,
        count: u64,
        bucket: std::time::Duration,
    ) -> Result<(RecvTiming, Vec<u64>, usize)> {
        if size > self.max_msg {
            bail!("message size {size} exceeds buffer {}", self.max_msg);
        }
        let t0 = Instant::now();
        let shares = self.shares(count);
        let parts = std::thread::scope(|scope| -> Result<Vec<_>> {
            let mut handles = Vec::new();
            for ((stream, buf), share) in self.streams.iter().zip(&mut self.rx).zip(shares) {
                handles.push(scope.spawn(move || -> Result<_> {
                    let mut s = stream;
                    let mut first = None;
                    let mut last = None;
                    let mut buckets: Vec<u64> = Vec::new();
                    let buf = &mut buf[..size];
                    for _ in 0..share {
                        s.read_exact(buf).context("data stream read")?;
                        let now = Instant::now();
                        first.get_or_insert(now);
                        last = Some(now);
                        let idx = (now.duration_since(t0).as_nanos()
                            / bucket.as_nanos().max(1)) as usize;
                        if idx >= buckets.len() {
                            buckets.resize(idx + 1, 0);
                        }
                        buckets[idx] += size as u64;
                    }
                    Ok((first.zip(last), buckets))
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        })?;
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
        let steady = steady_buckets(&timings, t0, bucket, all.len());
        Ok((merge_timings(timings, count), all, steady))
    }

    fn loaded_ping_initiator(
        &mut self,
        ping_size: usize,
        bulk_size: usize,
        bulk_count: u64,
    ) -> Result<Vec<u64>> {
        let (s0, bulk_streams) = self.streams.split_first().expect("at least one stream");
        let (tx0, bulk_tx) = self.tx.split_first_mut().expect("tx");
        let rx0 = &mut self.rx[0];
        if bulk_streams.is_empty() {
            bail!("loaded ping needs at least 2 streams");
        }
        let n = bulk_streams.len() as u64;
        let remaining = std::sync::atomic::AtomicUsize::new(bulk_streams.len());
        std::thread::scope(|scope| -> Result<Vec<u64>> {
            for (i, (stream, buf)) in bulk_streams.iter().zip(bulk_tx).enumerate() {
                let share = bulk_count / n + u64::from((i as u64) < bulk_count % n);
                let remaining = &remaining;
                scope.spawn(move || {
                    let r = tcp_send(stream, &buf[..bulk_size], share);
                    remaining.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    r
                });
            }
            let mut s = s0;
            let mut rtts = Vec::new();
            loop {
                let last = remaining.load(std::sync::atomic::Ordering::SeqCst) == 0;
                tx0[0] = u8::from(last);
                let t = Instant::now();
                s.write_all(&tx0[..ping_size])?;
                s.read_exact(&mut rx0[..ping_size])?;
                rtts.push(t.elapsed().as_nanos() as u64);
                if last {
                    break;
                }
            }
            tx0[0] = 0;
            s.write_all(&tx0[..ping_size])?; // drain message, not echoed
            Ok(rtts)
        })
    }

    fn reduce_begin(&mut self, _chunk: usize, _rounds: u32) -> Result<()> {
        Ok(())
    }

    fn reduce_round(&mut self, chunk: usize) -> Result<()> {
        // Send-then-recv is deadlock-safe only while the chunk fits in the
        // socket buffers; rounds are small by design.
        if chunk > 4 << 20 {
            bail!("reduce_round chunk {chunk} too large for the TCP path");
        }
        tcp_send(&self.streams[0], &self.tx[0][..chunk], 1)?;
        tcp_recv(&self.streams[0], &mut self.rx[0][..chunk], 1)?;
        Ok(())
    }

    fn loaded_ping_echoer(
        &mut self,
        ping_size: usize,
        bulk_size: usize,
        bulk_count: u64,
    ) -> Result<()> {
        let (s0, bulk_streams) = self.streams.split_first().expect("at least one stream");
        let (rx0, bulk_rx) = self.rx.split_first_mut().expect("rx");
        if bulk_streams.is_empty() {
            bail!("loaded ping needs at least 2 streams");
        }
        let n = bulk_streams.len() as u64;
        std::thread::scope(|scope| -> Result<()> {
            let mut handles = Vec::new();
            for (i, (stream, buf)) in bulk_streams.iter().zip(bulk_rx).enumerate() {
                let share = bulk_count / n + u64::from((i as u64) < bulk_count % n);
                handles.push(scope.spawn(move || tcp_recv(stream, &mut buf[..bulk_size], share)));
            }
            let mut s = s0;
            loop {
                s.read_exact(&mut rx0[..ping_size])?;
                let flag = rx0[0];
                let reply: &[u8] = &rx0[..ping_size];
                s.write_all(reply)?;
                if flag == 1 {
                    s.read_exact(&mut rx0[..ping_size])?; // drain
                    break;
                }
            }
            for h in handles {
                h.join().unwrap()?;
            }
            Ok(())
        })
    }
}
