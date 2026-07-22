# CPU-Grid

## Running/Monitoring...
![CPU-Grid in action](Screenshot-Main.png)

## Help (-h or --help)
![CPU-Grid Help](Screenshot-Help.png)

## What is it?
CPU-Grid is a real-time, terminal-based system monitoring tool written in Rust. It provides a clean, color-coded overview of your system's performance, including CPU frequencies, hardware temperatures, memory utilization, Zswap metrics, network throughput, storage telemetry, and user activity.

## Architecture & Compatibility
- **Architecture**: Full support for x86, ARM (incl. Apple Silicon), RISC-V, and IBM processor architectures via standard Linux kernel sysfs interfaces.
- **OS Enforcement**: Strictly targets Linux at compile time (`#![cfg(target_os = "linux")]`).

## Features

- **Real-time Monitoring**: Tracks per-core CPU frequency, RAM/Swap usage, hardware thermals, network speeds, storage I/O, and user idle time.
- **Advanced Storage Telemetry (New in v3.0.0)**: Monitors real-time disk read/write speeds (`R↑` / `W↓`) directly via `/proc/diskstats` and capacity via `libc::statvfs`. Features lifetime High Watermark (HR/HW) tracking for peak throughput.
- **Copy-on-Write (COW) Scaling**: Automatically detects BTRFS and ZFS filesystems, dynamically lowering the capacity color thresholds (85% vs 95%) to warn users of impending COW fragmentation degradation.
- **Smart Color-Coding**: Uses dynamic multi-stop color scaling and exponential lerping to visually represent load, speed, and temperature intensity.
- **Advanced Memory & Zswap Insights**: Monitors individual swap device utilization, Zswap compression algorithms (`zstd`, `lz4`, `lzo`, `deflate`), pool statistics, and compression ratios.
- **Network & Event Tracking**: Monitors active Rx/Tx interface speeds against maximum throughput and dynamically logs interface connection/disconnection events (`ACTIVATED` / `DEACTIVATED`).
- **User Activity Tracking**: Reads raw hardware system inputs (`/dev/input`, `/dev/pts`, `/dev/tty`) with secure user-space Wayland/X11 DBus fallbacks (`busctl`, `xprintidle`).
- **Virtual Machine Awareness**: Detects VM guest environments (via `/proc/cpuinfo` hypervisor flags and DMI product names) and adjusts the UI to hide inaccessible hardware limits.
- **Room Temperature**: Integrates with `temper-poll` to display ambient room temperature, including dynamic text warnings if room heat reaches critical levels (>=40°C).
- **Ultra-Lightweight & Safe**: Built with a multi-threaded architecture (7 dedicated worker threads: `cg-cpu`, `cg-thermal`, `cg-room`, `cg-mem`, `cg-net`, `cg-idle`, `cg-disk`) communicating via a bounded MPSC channel (capacity 32). Features zero-allocation data polling loops and strict thread cleanup for minimal CPU footprint and zero memory leaks.
- **Squashable Grid**: Designed to dynamically display logical core frequencies, temperatures, memory/swap pressure, networking, and disk layouts in the smallest shrunk terminal window possible.

## Color Legend
Color shade gradually changes between the ranges defined underneath.

| Metric | Color Scale |
| :--- | :--- |
| **CPU Freq** | Green (0-50%) → Yellow (50-70%) → Orange (70-85%) → Hot Red (85-100%) → Violet (>100% overclock) |
| **RAM Load** | Green (0-50%) → Yellow (50-70%) → Orange (70-85%) → Hot Red (85-95%) → Violet (>=95%)<br>*(Used and Available share the same color to indicate total memory pressure)* |
| **Swap Load** | Green (0-50%) → Yellow (50-70%) → Orange (70-80%) → Hot Red (80-90%) → Violet (>=90%) |
| **Network Load** | Green (Low) → Yellow → Orange → Hot Red (Near Interface Max) → Violet (Exceeds Theoretical) |
| **Storage Space** | Green (0-75%) → Yellow (75-85%) → Orange (85-90%) → Hot Red (90-95%) → Violet (>=95%)<br>*(Note: BTRFS/ZFS limits scale earlier to account for fragmentation)* |
| **Storage ↓↑** | Green (Baseline) → Yellow → Orange → Hot Red (Highest Known HW/HR) → Violet (Spiking to New Max) |
| **CPU Temp** | Green (Cool) → Red (Thermal Throttle Limit) → Violet (Exceeds Limit)<br>*(Note: Limit is dynamic, set by your specific CPU hardware)* |
| **Room Temp** | Green (<=24°C) → Yellow (27°C) → Orange (31°C) → LtRed (35°C) → Violet (>=40°C) |
| **Zswap Status** | Green (Enabled) → Bright Red (Disabled) → Yellow (Unknown Status) → Violet (Not Present) |
| **Zswap Algo** | Green (zstd) → Yellow (lz4) → Orange (lzo) → Red (deflate) → Violet (Other) |
| **Zswap Ratio** | Violet (<1:1) → Red (1:1) → Orange (1.5:1) → Yellow (2.5:1) → Green (4:1+) |
| **User Activity** | Cyan (Active) → Green → Yellow → Orange → Red → Violet (1+ Year Idle) |

## Requirements

- **OS**: Linux (Target strictly enforced at compile time. Requires access to `/proc` and `/sys/class/hwmon`).
- **Rust/Cargo**: Rust Edition 2024 required to build from source.
- **Dependencies**: `crossterm` (v0.29.0), `which` (v6.0), `libc` (v0.2).
- **External (OPTIONAL)**: `temper-poll` must be installed and in system PATH for ambient room temperature. Optional fallback binaries for user idle tracking: `busctl` (Wayland/GNOME/KDE) or `xprintidle` (X11).

## Command Line Arguments
Values are in seconds. Boundaries are strictly enforced.

- `-n, --cpu-stats-interval <secs>`: Interval for CPU stats (0.1 - 60s, default 2.0)
- `-r, --room-temp-interval <secs>`: Interval for Room Temp (1 - 3600s, default 2.0)
- `-m, --mem-stats-interval <secs>`: Interval for Memory stats (0.5 - 60s, default 2.0)
- `-t, --net-interval <secs>`: Interval for Network traffic (0.5 - 60s, default 2.0)
- `-d, --disk-interval <secs>`: Interval for Storage telemetry (0.5 - 60s, default 2.0)
- `-h, --help`: Prints help document
- `-v, --version`: Prints version and copyright

## Installation

1. Clone the repository:
   ```bash
   git clone https://github.com/StatusCode404/CPU-Grid.git
   cd cpu-grid
   cargo build --release
2. To run without Zswap monitoring...
   ```bash
   ./target/release/cpu_grid
3. To run with Zswap monitoring requires sudo privileges...
   ```bash
   sudo ./target/release/cpu_grid
4. If you have optional Temper USB thermometers for room temperature and you have temper-py (https://pypi.org/project/temper-py/) drivers.
    - temper-py installed only for $USER and not system-wide
      ```bash
      ./target/release/cpu_grid
    - temper-py installed only for $USER and not system-wide but with Zswap monitoring
      ```bash
      sudo -E ./target/release/cpu_grid
    - temper-py installed system-wide and with Zswap monitoring
      ```bash
      sudo ./target/release/cpu_grid

   
