# linkbench

One question, one pane: **how fast and useful is the link between these two
computers, in the context of common machine-learning tasks?**

Runs a suite of practical tests between two nodes and renders a single
report: raw microbenchmarks (latency, bandwidth sweep, message rate), then
the same numbers translated into ML terms — pipeline-parallel activation
hops, per-token tensor-parallel all-reduce, gradient sync, KV-cache
migration, weight streaming — with a verdict line for each workload.

Beyond averages, it measures how the link behaves under realistic
conditions:

- **Sustained timeline** — 8 s runs (3 s with `--quick`) bucketed at
  200 ms on the receive side, graphed with stability stats (worst dip,
  stall count). Stats cover only the steady window where every
  stream/QP is active; the wind-down tail after the first lane finishes
  is shown but not counted. Both nodes sample hwmon **temperatures**
  (hottest system sensor + NIC sensor) during these runs, overlaid on
  the graph — throttling shows up as heat rising while throughput falls.
- **Latency under load** — pings on lane 0 while all other lanes carry a
  full-rate bulk stream (serving tokens during a model transfer);
  reports RTT inflation vs idle.
- **After-idle wake-up** — pings following 1/10/100 ms pauses
  (compute-shaped gaps), exposing interrupt-coalescing, cwnd-decay, and
  power-state penalties that back-to-back benchmarks hide.

## The LinkBench2026 score

One number, 1.0–10.0, rating the link for paired-LLM hobbyist work:

- **1.0** — two nodes on 2.5GbE Ethernet: the bare minimum that works.
- **10.0** — an NVLink-bridged flagship pair (H200 NVL / GH200 class):
  the pinnacle a (moderately wealthy) hobbyist can buy on the open
  market. Hyperscaler-only fabric is out of scope by definition.

Two log-scaled axes between those era anchors — sustained bandwidth
(0.29 GB/s → 900 GB/s) and measured 16 KiB all-reduce latency (300 µs →
5 µs) — combined by geometric mean, so a link must be good at *both*;
imbalance is punished the way real workloads punish it (a sub-floor axis
clamps to a small epsilon and dominates without erasing the other).
Real-workload penalties then shave it: sustained stalls −15%, bufferbloat
under load −10%, after-idle wake-up lag −5%. The score is a link rating;
GPU compute is a separate axis for when GPU probing lands. Reference
points: 10GbE ≈ 2.5, TB4 ≈ 3.5, m5↔m6 TCP ≈ 3.6, hardware-RDMA 100GbE ≈
6, NVLink = 10. Anchors are frozen per era — hence the year in the name.

## Transports

- **tcp** — works between any two machines, no setup, multi-stream.
- **rdma** — RoCE / InfiniBand via libibverbs. Works with **hardware RDMA**
  (e.g. a ConnectX-6 Dx `rocep*` device) *and* with **software RoCE**
  (`rdma_rxe`) for machines that have no RDMA NIC — so the RDMA path is
  available to everyone, not just people with Mellanox cards. If a node
  has no hardware RDMA, `sudo linkbench roce-up --dev <iface>` brings up
  Soft-RoCE on any ordinary ethernet interface. `libibverbs.so.1` is
  dlopen'd at runtime, so the same binary runs on machines without
  rdma-core installed; no `-dev` packages needed anywhere.

  Soft-RoCE is CPU-bound and much slower than hardware RDMA (measured on
  the m5↔m6 pair: hardware = 2.4 µs one-way / 14.6 µs all-reduce; tuned
  Soft-RoCE = ~20 µs / ~60 µs) — but it lets any pair benchmark the RDMA
  transport, and the LinkBench2026 score reflects the real difference.

Planned: Thunderbolt/USB4 awareness, Intel Arc and NVIDIA CUDA
GPU-to-GPU staging (GPUDirect).

## GUI

`linkbench-gui` (in `gui/`) is the easy way to run it. Pick node A (runs
the test) and node B (serves) — each either "this machine" or an SSH
target like `dad@m5` — hit **Probe nodes**, check the discovered links
you want to measure, and press **Run Test**. Links appear in a table
with one row per link *mode* — RDMA-capable links contribute both a tcp
and an rdma row, since RDMA is just another mode of the same wire — and
all usable rows start checked. The batch tests each selection in
sequence and fills a comparison table (best value per column
highlighted, plus "best for bulk / per-token sync / bursty RPC" summary
lines); click any result row for its full detail pane. (Without a
probe, Run benchmarks B's SSH host directly.) The GUI scp-deploys the CLI binary to
the nodes, starts/stops the remote server for you, streams live progress,
and renders the dashboard: KPIs, latency table, bandwidth sweep, ML
scenarios, verdicts, and a context chart. The last run is remembered, as
is your node configuration. Requires passwordless SSH (BatchMode) for
remote nodes. `linkbench-gui --selftest` runs a headless local↔local
check of the whole pipeline.

### Probe & link discovery

Enter the two SSH targets and hit **Probe nodes**. The GUI deploys the
CLI, runs `linkbench probe --json` on both ends, and shows: each node's
hostname, passwordless-sudo state, CPU tuning, and thunderbolt host
peers — plus a **discovered links** list built by correlating the two
reports (matching subnets per interface kind: ethernet / connectx /
thunderbolt / wifi), each ping-tested from A. Click a link to configure
the benchmark against it (address + transport hint). Links that aren't
ready yet (e.g. a thunderbolt cable with no `thunderbolt-net` interface
or no IPv4) appear with a note about what's missing.

Thunderbolt links that need bring-up get a **⚡ bring up** button: it runs
`sudo linkbench tb-up --ip 10.111.11.{1,2}/24 --persist` on both nodes
(modprobe thunderbolt_net, address, largest MTU the driver takes —
65520 on tbnet — plus modules-load.d and a netplan file for boot
persistence), then re-probes. The same command works standalone for
agent-driven setups. Note: the first packets over a fresh thunderbolt
path take ~1 s (XDomain path setup) before settling to steady RTT.

If a node lacks passwordless sudo, a button stages
`~/toggle-passwordless-sudo.sh` on it — run that yourself (it
self-elevates, toggles a sudoers.d entry, prints the new state), then
re-probe.

## CLI usage

```sh
# node B (e.g. m6)
linkbench serve

# node A (e.g. m5)
linkbench run 10.10.10.2              # auto: picks RDMA if both ends have it
linkbench run 10.10.10.2 --transport tcp   # force TCP for comparison
linkbench run 10.10.10.2 --quick      # ~3x faster
linkbench run 10.10.10.2 --json > results.json

linkbench probe                       # what transports does this machine have?
sudo linkbench roce-up --dev eth0     # enable Soft-RoCE (no hardware RDMA needed)
sudo linkbench roce-up --dev eth0 --persist   # ...and keep it across reboots
linkbench history                     # past runs: score, metrics, tuning, notes
linkbench run … --notes "riser B"     # annotate the archived record

linkbench tune                        # show CPU tuning that affects the link
sudo linkbench tune --profile balanced  # performance governor, deep C-states off
sudo linkbench tune --profile latency   # also gate mid C-states (bursty RPC)
sudo linkbench tune --profile default   # restore powersave defaults
linkbench tune --json                 # machine-readable (for agent-driven tuning)
```

`tune` controls the levers that dominate link behaviour: frequency
governor, EPP, cpuidle gating, socket busy-polling
(`net.core.busy_read/busy_poll` — balanced/latency set 50 µs, which
takes C-state exits off the hot receive path and makes the latency
profile essentially free), and, with `--nic <iface>`, per-profile NIC
interrupt coalescing (measured sweet spot on mlx5: rx-usecs 3 /
rx-frames 32, adaptive off — collapses small-message p99 ~7x at zero
bandwidth cost). Profiles pick idle states by **exit-latency threshold**
(balanced: >100 µs off; latency: >10 µs off), so they work on any
machine. Measured on the
m5↔m6 pair: default→balanced cut after-idle RTT from ~1.4 ms to
~120 µs. Applying needs root and is runtime-only — persist it with a
oneshot systemd unit running `linkbench tune --profile balanced`. Every
run records both nodes' tuning summaries in the results, so runs are
comparable. The GUI's advanced section can apply a profile to both
nodes (via `sudo -n`) before each run.

Useful flags: `--device mlx5_0`, `--gid-index N` (default auto-picks a
RoCE v2 IPv4-mapped GID), `--streams N` (TCP parallelism, default 4),
`--qps N` (parallel RDMA queue pairs, default 4 — big win on CPU-bound
Soft-RoCE; the server follows the client's count), `--duration secs`
(per-test target, default 1.0), `--region-mb` (buffer region size,
default 128; max message = region/QPs on the rdma path).

## Run history

Every completed run is archived as a full JSON record in
`~/.local/share/linkbench/history/` on the machine that orchestrated it
(the GUI archives its runs with the link·mode label; manual CLI runs
archive themselves). Each record carries the complete results — score,
all metrics, transport, path, and both nodes' CPU-tuning summaries —
plus optional free-form `--notes` ("riser B", "new DAC cable"), making
the archive an experiment log for tuning and hardware comparisons.
`linkbench history` prints a comparison table; `--json` emits the full
records for analysis.

## Methodology

- **Rates** are receive-side, first→last message completion, counting
  messages 2..N — immune to clock skew and sender ramp-up.
- **Sustained rates** are computed over the *steady window* only (buckets
  where every stream/QP is still active); the wind-down tail after the
  first lane finishes is static-share skew, shown on the graph but never
  counted.
- **Latencies are medians** (p50, with p99 alongside). Means are never
  used for latency-class metrics: the distributions have heavy scheduler
  tails that real, pipelined implementations don't pay per operation.
- **The 16 KiB all-reduce pre-posts receives across rounds** (as NCCL/Gloo
  do), so no round can hit receiver-not-ready backoff; the metric is the
  median round ×2.
- Known bounded biases: bidir and sweep points still use full first→last
  spans (≤ ~3% wind-down bias — lanes differ by at most one message); the
  after-idle test measures one-sided wake-up (the echoer busy-polls and
  never sleeps), so real two-sided idle is somewhat worse than reported;
  each sweep point is a single trial, indicative rather than definitive.
- The idle-gap p99 is only meaningful for the short gaps; at 100 ms it is
  effectively "max observed".

## Notes

- **Locked memory**: RDMA registers 2×128 MiB of pinned buffers. If
  `ibv_reg_mr` fails, raise `ulimit -l` (memlock) for the user.
- Bandwidth numbers are measured on the **receive side** between first and
  last message completion, so nothing depends on clock sync between nodes.
- The all-reduce tests move real bytes both directions **and** run a real
  f32 reduction pass, so the "effective" number includes CPU cost, like a
  real 2-node data-parallel step.
- The control channel is plain TCP (port 7842) carrying JSON; benchmark
  payload flows on a separate data plane (extra TCP streams or an RDMA RC
  queue pair using SEND/RECV).

## Design

```
src/main.rs       CLI: serve / run / probe
src/proto.rs      control-channel protocol (newline JSON over TCP)
src/transport.rs  DataPlane trait + TCP implementation
src/rdma/ffi.rs   minimal hand-rolled libibverbs FFI (dlopen, no headers)
src/rdma/mod.rs   RDMA RC data plane (slotted send/recv over registered MRs)
src/bench.rs      test choreography + results model
src/report.rs     the single pane
```

New transports implement the `DataPlane` trait (`send_burst`,
`recv_burst`, `bidir_burst`, `ping`, `echo`) plus a setup handshake in
`main.rs`/`proto.rs`; the bench and report layers are transport-agnostic.
