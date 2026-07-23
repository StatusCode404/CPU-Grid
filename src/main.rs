
#![cfg(target_os = "linux")]

use crossbeam_channel::{self, Receiver, Sender};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
    QueueableCommand,
};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::{self, Write as _}; // Required for zero-allocation string buffer write! macros
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Default polling interval in seconds for background telemetry threads.
const DEFAULT_INTERVAL: f64 = 2.0;

/// Minimum TUI cell width allowed before warning rendering triggers.
const MIN_CELL_WIDTH: usize = 17;

/// Atomic flag disabling repetitive failing subprocess calls for desktop idle time.
static DESKTOP_IDLE_DISABLED: AtomicBool = AtomicBool::new(false);

// --- Data Structures ---

/// Holds individual CPU sensor label, temperature value, rendered color ANSI sequence, and parent status flag.
struct TempStat {
    label: String,
    val: f64,
    color: String,
    is_parent: bool,
}

/// Blueprint for a pre-discovered hardware thermal sensor to prevent VFS traversal overhead.
#[derive(Clone)]
struct ThermalSensorDef {
    parent_name: String,
    label: String,
    input_path: String,
    limit: f64,
    is_parent: bool,
}

/// Dynamic tracker for high-watermark storage traffic peaks and visual blink timers.
struct StorageHrHwState {
    current_r: f64,
    pending_r: f64,
    pending_time_r: Instant,
    last_r_peak: Option<Instant>,
    current_w: f64,
    pending_w: f64,
    pending_time_w: Instant,
    last_w_peak: Option<Instant>,
}

impl StorageHrHwState {
    /// Constructs a clean storage peak state tracker initialized with boundary extremes.
    fn new() -> Self {
        let now = Instant::now();
        Self {
            current_r: 0.0,
            pending_r: 0.0,
            pending_time_r: now,
            last_r_peak: None,
            current_w: 0.0,
            pending_w: 0.0,
            pending_time_w: now,
            last_w_peak: None,
        }
    }

    /// Evaluates incoming read/write throughput rates and updates highest recorded peaks and blink timers safely.
    fn update(&mut self, r: f64, w: f64) {
        if r > self.pending_r {
            self.pending_r = r;
            self.pending_time_r = Instant::now();
        }
        // Absolute max logic: once current_r/w is established, it strictly holds the highest recorded value.
        // Once locked in after the 1.5s delay, the `last_r_peak` triggers the 3.0s UI blinker.
        if self.pending_r > self.current_r && self.pending_time_r.elapsed().as_secs_f64() >= 1.5 {
            self.current_r = self.pending_r;
            self.last_r_peak = Some(Instant::now());
        }

        if w > self.pending_w {
            self.pending_w = w;
            self.pending_time_w = Instant::now();
        }
        if self.pending_w > self.current_w && self.pending_time_w.elapsed().as_secs_f64() >= 1.5 {
            self.current_w = self.pending_w;
            self.last_w_peak = Some(Instant::now());
        }
    }
}

/// General peak state tracker with timestamp monitoring for 2Hz blink animations and persistent lerp coloring.
struct ValuePeakTracker {
    max_val: f64,
    min_val: f64,
    max_color: String,
    min_color: String,
    last_max_peak: Option<Instant>,
    last_min_peak: Option<Instant>,
}

impl ValuePeakTracker {
    /// Constructs a clean general peak tracker initialized with boundary extremes and default white text.
    fn new() -> Self {
        Self {
            max_val: f64::MIN,
            min_val: f64::MAX,
            max_color: "\x1b[1;37m".to_string(),
            min_color: "\x1b[1;37m".to_string(),
            last_max_peak: None,
            last_min_peak: None,
        }
    }

    /// Updates maximum recorded value and color mapping, triggering a fresh 3.0s blink window if exceeded.
    fn update_max(&mut self, val: f64, color: &str) -> bool {
        if val > self.max_val && self.max_val != f64::MIN {
            self.max_val = val;
            self.max_color = color.to_string();
            self.last_max_peak = Some(Instant::now());
            true
        } else if self.max_val == f64::MIN {
            self.max_val = val;
            self.max_color = color.to_string();
            false
        } else {
            false
        }
    }

    /// Updates minimum recorded value and color mapping, triggering a fresh 3.0s blink window if lower.
    fn update_min(&mut self, val: f64, color: &str) -> bool {
        if val < self.min_val && self.min_val != f64::MAX {
            self.min_val = val;
            self.min_color = color.to_string();
            self.last_min_peak = Some(Instant::now());
            true
        } else if self.min_val == f64::MAX {
            self.min_val = val;
            self.min_color = color.to_string();
            false
        } else {
            false
        }
    }
}

/// Container struct representing raw disk layout capabilities for the UI loop formatters.
#[derive(Clone)]
struct DiskNodeRaw {
    mount_padded: String,
    used_bytes: u64,
    total_bytes: u64,
    capacity_color: String,
    r_speed: f64,
    w_speed: f64,
    raw_dev_name: String,
}

/// Extensible MPMC inter-thread channel message enumeration supporting UI events, telemetry, and future features.
enum Msg {
    CpuFreqs(Vec<f64>),
    CpuTemps(Vec<(String, Vec<TempStat>)>),
    RoomTemp(Option<f64>),
    MemStats {
        ram_str: String,
        zswap_str: String,
        swap_total_str: String,
        swaps_formatted: Vec<String>,
    },
    NetStats(Vec<(String, f64, f64, f64)>),
    NetEvent(String, String),
    DiskStats {
        disk_nodes: Vec<DiskNodeRaw>,
        agg_total: u64,
        agg_used: u64,
    },
    UserIdle(Duration),
    InputEvent(Event),
    Tick,
}

/// Primary UI state container holding active metrics, formatted render strings, historical peaks, and dirty state flags.
struct SystemState {
    cpu_model_display: String,
    limits: (Option<f64>, Option<f64>),
    highest_overclock: Option<f64>,
    hoc_last_peak: Option<Instant>,
    freqs: Vec<f64>,
    cpu_temps: Vec<(String, Vec<TempStat>)>,
    parent_ht_trackers: HashMap<String, ValuePeakTracker>,
    room_temp_val: Option<f64>,
    room_temp_tracker: ValuePeakTracker,
    last_room_hot_time: Option<Instant>,
    mem_ram_str: String,
    mem_zswap_str: String,
    mem_swap_total_str: String,
    swaps_formatted: Vec<String>,
    net_events: HashMap<String, (String, Instant)>,

    // Net UI state arrays
    raw_net_nodes: Vec<(String, f64, f64, f64)>,
    net_trackers: HashMap<String, (ValuePeakTracker, ValuePeakTracker)>,
    net_total_tracker: (ValuePeakTracker, ValuePeakTracker),
    net_total_rx: f64,
    net_total_tx: f64,
    net_total_max: f64,

    // Disk UI state arrays
    disk_nodes: Vec<DiskNodeRaw>,
    disk_trackers: HashMap<String, StorageHrHwState>,
    disk_global_hw: StorageHrHwState,
    disk_agg_total: u64,
    disk_agg_used: u64,
    disk_agg_read: f64,
    disk_agg_write: f64,
    idle_time: Duration,

    // Render-time zero-allocation string buffers
    net_total_str: String,
    display_room_temp: String,
    net_display_pool: Vec<String>,
    disk_cap_parent: String,
    disk_io_parent: String,
    disk_combined_parent: String,
    disk_cap_pool: Vec<String>,
    disk_io_pool: Vec<String>,
    disk_combined_pool: Vec<String>,
    disk_io_split_pool: Vec<String>,

    // Reactive Dirty Rendering Flag
    screen_is_dirty: bool,
}

// --- Display & Formatting Helpers ---

/// Prints CLI usage documentation, command flags, High-Watermark definitions, and Room Temp alert behavior.
fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    let rst = "\x1b[0m";
    let grn = "\x1b[38;2;0;200;0m";
    let yel = "\x1b[38;2;255;255;0m";
    let org = "\x1b[38;2;255;165;0m";
    let red = "\x1b[38;2;255;0;0m";
    let vio = "\x1b[38;2;238;130;238m";
    let cya = "\x1b[38;2;0;255;255m";
    let ltr = "\x1b[38;2;255;100;100m";
    let wht = "\x1b[1;37m";

    println!("\x1b[1;38;2;255;215;0mCPU-Grid ver:{}\x1b[0m", version);
    println!("Copyright (C) 2026 StatusCode404 https://github.com/StatusCode404");
    println!("Project: https://github.com/StatusCode404/CPU-Grid");
    println!("Compatibility: Full support for x86, ARM (incl. Apple Silicon), RISC-V, and IBM processor architectures via standard Linux kernel sysfs interfaces.");

    println!("\nUsage (Values are in seconds. Parameters given less than or greater than the boundary ranges will fall back to the nearest boundary range.):");
    println!("  -n, --cpu-stats-interval <secs>    Interval for CPU stats (0.1 - 60s, default 2.0)");
    println!("  -r, --room-temp-interval <secs>    Interval for Room Temp (1 - 3600s, default 2.0)");
    println!("  -m, --mem-stats-interval <secs>    Interval for Memory stats (0.5 - 60s, default 2.0)");
    println!("  -t, --net-interval <secs>          Interval for Network traffic (0.5 - 60s, default 2.0)");
    println!("  -d, --disk-interval <secs>         Interval for Storage telemetry (0.5 - 60s, default 2.0)");
    println!("  -h, --help                         Prints this help document");
    println!("  -v, --version                      Prints version and copyright");

    println!("\nTips:");
    println!("  If running with {red}sudo{rst} and Room Temp fails, use '{red}sudo -E{rst}' to preserve your user environment.");

    println!("\nColor Legend (Color shade gradually changes between the ranges defined underneath):");
    println!("  CPU Freq:       {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-70%) -> {org}Orange{rst}(70-85%) -> {red}Hot Red{rst}(85-100%) -> {vio}Violet{rst}(>100% overclock)");
    println!("  RAM Load:       {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-70%) -> {org}Orange{rst}(70-85%) -> {red}Hot Red{rst}(85-95%) -> {vio}Violet{rst}(>=95%)");
    println!("                  (Used and Available values share the same color to indicate total memory pressure)");
    println!("  Swap Load:      {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-70%) -> {org}Orange{rst}(70-80%) -> {red}Hot Red{rst}(80-90%) -> {vio}Violet{rst}(>=90%)");
    println!("  Network Load:   {grn}Green{rst}(Low) -> {yel}Yellow{rst} -> {org}Orange{rst} -> {red}Hot Red{rst}(Near Interface Max) -> {vio}Violet{rst}(Exceeds Theoretical)");
    println!("  Storage Space:  {grn}Green{rst}(0-75%) -> {yel}Yellow{rst}(75-85%) -> {org}Orange{rst}(85-90%) -> {red}Hot Red{rst}(90-95%) -> {vio}Violet{rst}(>=95%)");
    println!("                  (Note: BTRFS/ZFS limits scale earlier to account for Copy-on-Write fragmentation degradation)");
    println!("  Storage \x1b[1m↓↑\x1b[0m:     {grn}Green{rst}(Baseline) -> {yel}Yellow{rst} -> {org}Orange{rst} -> {red}Hot Red{rst}(Highest Known HW/HR) -> {vio}Violet{rst}(Spiking to New Max)");
    println!("  CPU Temp:       {grn}Green{rst} (Cool) -> {red}Red{rst} (Thermal Throttle Limit) -> {vio}Violet{rst} (Exceeds Limit)");
    println!("  Room Temp:      {grn}Green{rst} (<=24) -> {yel}Yellow{rst}(27) -> {org}Orange{rst}(31) -> {ltr}LtRed{rst}(35) -> {vio}Violet{rst}(>=40)");
    println!("  Zswap Status:   {grn}Green{rst} (Enabled) -> {red}Bright Red{rst} (Disabled) -> {yel}Yellow{rst} (Unknown Status) -> {vio}Violet{rst} (Not Present)");
    println!("  Zswap Algo:     {grn}zstd{rst} (Best) -> {yel}lz4{rst} -> {org}lzo{rst} -> {red}deflate{rst} -> {vio}Other{rst}");
    println!("  Zswap Ratio:    {vio}Violet{rst} (<1:1) -> {red}Red{rst} (1:1) -> {org}Orange{rst} (1.5:1) -> {yel}Yellow{rst} (2.5:1) -> {grn}Green{rst} (4:1+)");
    println!("  User Activity:  {cya}Cyan{rst} (Active) -> {grn}Green{rst} -> {yel}Yellow{rst} -> {org}Orange{rst} -> {red}Red{rst} -> {vio}Violet{rst} (1+ Year Idle)");

    println!("\nHigh-Watermark Stats Legend (Peak records strobe at 2Hz for 3s upon breaking record):");
    println!("  {wht}HOC:{rst}  Highest Overclock frequency recorded across all CPU cores. (Dynamically hidden; this metric will only appear if an overclock is actively achieved).");
    println!("  {wht}HT:{rst}   Highest Temperature recorded for a thermal zone parent package/chiplet.");
    println!("  {wht}LRT:{rst}  Lowest Room Temperature recorded since application launch.");
    println!("  {wht}HRT:{rst}  Highest Room Temperature recorded since application launch.");
    println!("  {wht}H↓:{rst}   Highest Download (Receive) network bandwidth recorded.");
    println!("  {wht}H↑:{rst}   Highest Upload (Transmit) network bandwidth recorded.");
    println!("  {wht}HR:{rst}   Highest Read throughput rate recorded for a storage device.");
    println!("  {wht}HW:{rst}   Highest Write throughput rate recorded for a storage device.");

    println!("\nRoom Temperature Monitor (Optional TEMPer USB Thermometer):");
    println!("  Requires a compatible TEMPer USB device and 'temper-poll' executable in PATH.");
    println!("  Displays live room ambient temperature alongside LRT (Lowest) and HRT (Highest) records.");
    println!("  Warning Threshold Behavior:");
    println!("    - Reaching or exceeding 40.0°C (104.0°F) triggers an ambient overheat alert.");
    println!("    - Displays an urgent warning banner advising thermal shutdown considerations.");
    println!("    - The overheat alert banner remains actively displayed for 5 minutes (300s) post-event.");
}

/// Generates a clock-synchronized boolean toggle for 2 Hz blinking (250ms state duration).
#[inline]
fn get_blink_toggle() -> bool {
    (SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        / 250)
        % 2
        == 0
}

/// Evaluates if a target peak timestamp is actively inside the 3.0-second 2 Hz blink window.
#[inline]
fn is_blink_active(last_peak: Option<Instant>) -> bool {
    if let Some(t) = last_peak {
        t.elapsed().as_secs_f64() < 3.0
    } else {
        false
    }
}

/// Determines whether any metric in the system is actively blinking, signaling the 250ms ticker to redraw.
fn is_any_blink_active(state: &SystemState) -> bool {
    if is_blink_active(state.hoc_last_peak) {
        return true;
    }
    if is_blink_active(state.room_temp_tracker.last_min_peak) || is_blink_active(state.room_temp_tracker.last_max_peak) {
        return true;
    }
    for tracker in state.parent_ht_trackers.values() {
        if is_blink_active(tracker.last_max_peak) {
            return true;
        }
    }
    for trackers in state.net_trackers.values() {
        if is_blink_active(trackers.0.last_max_peak) || is_blink_active(trackers.1.last_max_peak) {
            return true;
        }
    }
    if is_blink_active(state.net_total_tracker.0.last_max_peak) || is_blink_active(state.net_total_tracker.1.last_max_peak) {
        return true;
    }
    for tracker in state.disk_trackers.values() {
        if is_blink_active(tracker.last_r_peak) || is_blink_active(tracker.last_w_peak) {
            return true;
        }
    }
    if is_blink_active(state.disk_global_hw.last_r_peak) || is_blink_active(state.disk_global_hw.last_w_peak) {
        return true;
    }
    false
}

/// Calculates visual character length of a string while filtering out standard ANSI terminal escape sequences.
fn strip_ansi(s: &str) -> usize {
    let mut len = 0;
    let mut in_ansi = false;
    for c in s.chars() {
        if c == '\x1b' { in_ansi = true; }
        else if in_ansi && c == 'm' { in_ansi = false; }
        else if !in_ansi { len += 1; }
    }
    len
}

// --- ZERO-ALLOCATION DISPLAY FORMATTERS ---

/// Formats a raw byte integer into a precise 10-character width string (e.g. `  12.3  GB`).
struct FormatSize(u64);
impl fmt::Display for FormatSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kb = 1024_f64;
        let mb = kb * 1024.0;
        let gb = mb * 1024.0;
        let tb = gb * 1024.0;
        let pb = tb * 1024.0;
        let val = self.0 as f64;

        let (v, u) = if val < kb { (val, " B") }
        else if val < mb { (val / kb, "KB") }
        else if val < gb { (val / mb, "MB") }
        else if val < tb { (val / gb, "GB") }
        else if val < pb { (val / tb, "TB") }
        else { (val / pb, "PB") };

        if v >= 1000.0 { write!(f, "{:6.0} {}", v, u) }
        else { write!(f, "{:6.1} {}", v, u) }
    }
}

/// Formats a network bandwidth integer into a precise alignment.
struct FormatNetSpeed(f64);
impl fmt::Display for FormatNetSpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kb = 1024_f64;
        let mb = kb * 1024.0;
        let gb = mb * 1024.0;
        let tb = gb * 1024.0;
       
        let bps = self.0;
        if bps < kb { write!(f, "{:6.1}   B/s", bps) }
        else if bps < mb { write!(f, "{:6.1}  KB/s", bps / kb) }
        else if bps < gb { write!(f, "{:6.1}  MB/s", bps / mb) }
        else if bps < tb { write!(f, "{:6.2}  GB/s", bps / gb) }
        else { write!(f, "{:6.2}  TB/s", bps / tb) }
    }
}

/// Formats a disk bandwidth integer dropping precision for grid real estate (exact 9 chars).
struct FormatDiskSpeed(f64);
impl fmt::Display for FormatDiskSpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kb = 1024_f64;
        let mb = kb * 1024.0;
        let gb = mb * 1024.0;
        let tb = gb * 1024.0;
        let pb = tb * 1024.0;

        let (v, u) = if self.0 < kb { (self.0, "B/s") }
        else if self.0 < mb { (self.0 / kb, "K/s") }
        else if self.0 < gb { (self.0 / mb, "M/s") }
        else if self.0 < tb { (self.0 / gb, "G/s") }
        else if self.0 < pb { (self.0 / tb, "T/s") }
        else { (self.0 / pb, "P/s") };

        if v >= 1000.0 { write!(f, "{:5.0} {}", v, u) }
        else { write!(f, "{:5.1} {}", v, u) }
    }
}

/// Formats an idle duration integer into a human-readable days/hours/mins string.
struct FormatIdleTime(u64);
impl fmt::Display for FormatIdleTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0;
        let y = secs / 31536000;
        let mon = (secs % 31536000) / 2592000;
        let d = (secs % 2592000) / 86400;
        let h = (secs % 86400) / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;

        if y > 0 { write!(f, "{} Years {} Months {} Days {:02}:{:02}:{:02}", y, mon, d, h, m, s) }
        else if mon > 0 { write!(f, "{} Months {} Days {:02}:{:02}:{:02}", mon, d, h, m, s) }
        else if d > 0 { write!(f, "{} Days {:02}:{:02}:{:02}", d, h, m, s) }
        else if h > 0 { write!(f, "{:02}:{:02}:{:02}", h, m, s) }
        else if m > 0 { write!(f, "{:02}:{:02}", m, s) }
        else { write!(f, "{}s", s) }
    }
}

/// Formats highest overclock frequency into exact 5-character length dynamically scaling decimals.
struct FormatHoc(f64);
impl fmt::Display for FormatHoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let khz = self.0 * 1000.0;
        if khz >= 1_000_000_000_000.0 {
            write!(f, "{:4.0}P", (khz / 1_000_000_000_000.0).ceil())
        } else if khz >= 1_000_000_000.0 {
            write!(f, "{:4.0}T", (khz / 1_000_000_000.0).ceil())
        } else if khz >= 1_000_000.0 {
            let ghz = self.0 / 1000.0;
            if ghz < 10.0 { write!(f, "{:4.2}G", (ghz * 100.0).ceil() / 100.0) }
            else if ghz < 100.0 { write!(f, "{:4.1}G", (ghz * 10.0).ceil() / 10.0) }
            else { write!(f, "{:4.0}G", ghz.ceil()) }
        } else if khz >= 1000.0 {
            write!(f, "{:4.0}M", self.0.ceil())
        } else {
            write!(f, "{:4.0}K", khz.ceil())
        }
    }
}

/// Formats simple float metrics strictly into exactly 6 characters.
struct FormatDynamic6(f64);
impl fmt::Display for FormatDynamic6 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let int_part = self.0.trunc();
        let int_len = if int_part == 0.0 { 1 } else { int_part.abs().log10().floor() as i32 + 1 };
       
        if int_len >= 6 { write!(f, "{:6.0}", self.0.clamp(0.0, 999999.0)) }
        else { let prec = (6 - int_len - 1).max(0) as usize; write!(f, "{:.*}", prec, self.0) }
    }
}

/// [SIMD Optimization]: #[inline(always)] forces this heavily utilized math function
/// to be directly injected into mapping closures. When iterating over arrays (like CPU cores),
/// LLVM can auto-vectorize the lerp calculations across multiple elements simultaneously via SIMD hardware.
#[inline(always)]
fn lerp_color(c1: (u8, u8, u8), c2: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    (
        (c1.0 as f64 + (c2.0 as f64 - c1.0 as f64) * t).round() as u8,
        (c1.1 as f64 + (c2.1 as f64 - c1.1 as f64) * t).round() as u8,
        (c1.2 as f64 + (c2.2 as f64 - c1.2 as f64) * t).round() as u8,
    )
}

/// Formats CPU frequency utilization dynamically mapped into ANSI RGB gradients.
#[inline]
fn get_cpu_color(t: f64) -> String {
    if t >= 1.0 { return "\x1b[1;38;2;238;130;238m".to_string(); }
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t <= 0.5 { lerp_color((0, 200, 0), (255, 255, 0), t / 0.5) }
    else if t <= 0.7 { lerp_color((255, 255, 0), (255, 165, 0), (t - 0.5) / 0.2) }
    else if t <= 0.85 { lerp_color((255, 165, 0), (200, 30, 30), (t - 0.7) / 0.15) }
    else { lerp_color((200, 30, 30), (255, 0, 0), (t - 0.85) / 0.15) };              
   
    if t >= 0.85 { format!("\x1b[1;38;2;{};{};{}m", r, g, b) }
    else { format!("\x1b[38;2;{};{};{}m", r, g, b) }
}

/// Formats memory load percentages into capacity warning gradients.
#[inline]
fn get_ram_color(t: f64) -> String {
    if t >= 0.95 { return "\x1b[1;38;2;238;130;238m".to_string(); }
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t <= 0.5 { lerp_color((0, 200, 0), (255, 255, 0), t / 0.5) }
    else if t <= 0.7 { lerp_color((255, 255, 0), (255, 165, 0), (t - 0.5) / 0.2) }
    else if t <= 0.85 { lerp_color((255, 165, 0), (200, 30, 30), (t - 0.7) / 0.15) }
    else { lerp_color((200, 30, 30), (255, 0, 0), (t - 0.85) / 0.10) };
   
    if t >= 0.85 { format!("\x1b[1;38;2;{};{};{}m", r, g, b) }
    else { format!("\x1b[38;2;{};{};{}m", r, g, b) }
}

/// Colors Swap utilization based dynamically on thresholds.
#[inline]
fn get_swap_color(t: f64) -> String {
    if t >= 0.90 { return "\x1b[1;38;2;238;130;238m".to_string(); }
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t <= 0.5 { lerp_color((0, 200, 0), (255, 255, 0), t / 0.5) }
    else if t <= 0.7 { lerp_color((255, 255, 0), (255, 165, 0), (t - 0.5) / 0.2) }
    else if t <= 0.8 { lerp_color((255, 165, 0), (200, 30, 30), (t - 0.7) / 0.1) }
    else { lerp_color((200, 30, 30), (255, 0, 0), (t - 0.8) / 0.1) };
   
    if t >= 0.8 { format!("\x1b[1;38;2;{};{};{}m", r, g, b) }
    else { format!("\x1b[38;2;{};{};{}m", r, g, b) }
}

/// Scales disk capacity colors smartly accommodating early degradation markers for BTRFS and ZFS.
#[inline]
fn get_disk_capacity_color(percent: f64, is_cow: bool) -> String {
    let (t_green, t_yellow, t_orange, t_red) = if is_cow {
        (0.70, 0.80, 0.90, 0.95) // Adjusted COW threshold to keep 50%+ safe and green
    } else {
        (0.80, 0.90, 0.95, 0.98) // Standard formats like Ext4/XFS
    };

    if percent >= t_red { return "\x1b[1;38;2;238;130;238m".to_string(); } // Violet
    let t = percent.clamp(0.0, 1.0);
   
    let (r, g, b) = if t <= t_green { (0, 200, 0) }
    else if t <= t_yellow { lerp_color((0, 200, 0), (255, 255, 0), (t - t_green) / (t_yellow - t_green)) }
    else if t <= t_orange { lerp_color((255, 255, 0), (255, 165, 0), (t - t_yellow) / (t_orange - t_yellow)) }
    else { lerp_color((255, 165, 0), (255, 0, 0), (t - t_orange) / (t_red - t_orange)) };
   
    if t >= t_orange { format!("\x1b[1;38;2;{};{};{}m", r, g, b) }
    else { format!("\x1b[38;2;{};{};{}m", r, g, b) }
}

/// Exponentially maps current disk throughput against known system physical limits (peaks).
#[inline]
fn get_exp_disk_speed_color(speed: f64, current_hw: f64) -> String {
    if speed > current_hw && current_hw > 1024.0 { return "\x1b[1;38;2;238;130;238m".to_string(); }
    if speed >= current_hw * 0.98 && current_hw > 1024.0 { return "\x1b[1;31m".to_string(); }
   
    let t = if current_hw > 0.0 { (speed / current_hw).clamp(0.0, 1.0).powf(3.0) } else { 0.0 };
   
    let (r, g, b) = if t <= 0.4 { lerp_color((0, 200, 0), (255, 255, 0), t / 0.4) }
    else if t <= 0.8 { lerp_color((255, 255, 0), (255, 165, 0), (t - 0.4) / 0.4) }
    else { lerp_color((255, 165, 0), (200, 30, 30), (t - 0.8) / 0.2) };
   
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

/// Evaluates real-time packet transit arrays cleanly against mapped physical line hardware limits.
#[inline]
fn get_net_color(speed: f64, max_speed: f64) -> String {
    let t = speed / max_speed.max(1.0);
    if t > 1.0 { return "\x1b[1;38;2;238;130;238m".to_string(); }
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t <= 0.33 { lerp_color((0, 200, 0), (255, 255, 0), t / 0.33) }
    else if t <= 0.66 { lerp_color((255, 255, 0), (255, 165, 0), (t - 0.33) / 0.33) }
    else if t <= 0.83 { lerp_color((255, 165, 0), (200, 30, 30), (t - 0.66) / 0.17) }
    else { lerp_color((200, 30, 30), (255, 0, 0), (t - 0.83) / 0.17) };
   
    if t >= 0.83 { format!("\x1b[1;38;2;{};{};{}m", r, g, b) }
    else { format!("\x1b[38;2;{};{};{}m", r, g, b) }
}

/// Maps input bus polling timeouts against year-long inactivity scales seamlessly.
#[inline]
fn get_idle_color(secs: u64) -> String {
    let t = secs as f64 / 31536000.0;
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t <= 0.25 { lerp_color((0, 200, 0), (255, 255, 0), t / 0.25) }
    else if t <= 0.5 { lerp_color((255, 255, 0), (255, 165, 0), (t - 0.25) / 0.25) }
    else if t <= 0.75 { lerp_color((255, 165, 0), (255, 0, 0), (t - 0.5) / 0.25) }
    else { lerp_color((255, 0, 0), (238, 130, 238), (t - 0.75) / 0.25) };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

/// Converts external thermometer sensor integers directly into human-readable gradient strings.
#[inline]
fn get_room_temp_color(temp: f64) -> String {
    let (r, g, b) = if temp <= 24.0 { (0, 200, 0) }
    else if temp <= 27.0 { lerp_color((0, 200, 0), (255, 255, 0), (temp - 24.0) / 3.0) }
    else if temp <= 31.0 { lerp_color((255, 255, 0), (255, 165, 0), (temp - 27.0) / 4.0) }
    else if temp <= 35.0 { lerp_color((255, 165, 0), (255, 100, 100), (temp - 31.0) / 4.0) }
    else if temp < 40.0 { lerp_color((255, 100, 100), (238, 130, 238), (temp - 35.0) / 5.0) }
    else { (238, 130, 238) };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

/// Computes Zswap utilization mapping metrics directly into inline gradient outputs.
#[inline]
fn get_ratio_color(ratio: f64) -> String {
    let (r, g, b) = if ratio < 1.0 { (238, 130, 238) }
    else if ratio <= 1.5 { lerp_color((255, 50, 0), (255, 165, 0), (ratio - 1.0) / 0.5) }
    else if ratio <= 2.5 { lerp_color((255, 165, 0), (255, 255, 0), (ratio - 1.5) / 1.0) }
    else if ratio <= 4.0 { lerp_color((255, 255, 0), (0, 200, 0), (ratio - 2.5) / 1.5) }
    else { (0, 200, 0) };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

/// Identifies dynamic Zswap algorithms mapping native strings purely to performance hues.
#[inline]
fn get_zswap_algo_color(algo: &str) -> String {
    match algo {
        "zstd" => "\x1b[38;2;0;200;0m".to_string(),
        "lz4" => "\x1b[38;2;255;255;0m".to_string(),
        "lzo" => "\x1b[38;2;255;165;0m".to_string(),
        "deflate" => "\x1b[38;2;255;0;0m".to_string(),
        _ => "\x1b[38;2;238;130;238m".to_string(),
    }
}

/// Formats a visual watermark string dynamically into a raw String buffer, toggling colors during active blink cycles.
/// Integrates Zero-Allocation Display formatters dynamically.
fn write_watermark_buf(
    buf: &mut String,
    key: &str,
    val_fmt: &dyn std::fmt::Display,
    target_color: &str,
    is_blinking: bool,
    toggle: bool,
    key_stays_target: bool,
) {
    let white_bold = "\x1b[1;37m";
    let rst = "\x1b[0m";

    if is_blinking {
        if toggle {
            let _ = write!(buf, "{}\x1b[1m{} {}\x1b[1m{}{}", target_color, key, target_color, val_fmt, rst);
        } else {
            let _ = write!(buf, "{}{} {}{}{}", white_bold, key, white_bold, val_fmt, rst);
        }
    } else {
        if key_stays_target {
            let _ = write!(buf, "{}\x1b[1m{} {}\x1b[1m{}{}", target_color, key, target_color, val_fmt, rst);
        } else {
            let _ = write!(buf, "{}{} {}\x1b[1m{}{}", white_bold, key, target_color, val_fmt, rst);
        }
    }
}

/// Formats a visual watermark string inline directly into the standard output stream for CPU Freq/Temp arrays safely.
fn write_watermark_inline(
    stdout: &mut BufWriter<io::Stdout>,
    key: &str,
    val_fmt: &dyn std::fmt::Display,
    target_color: &str,
    is_blinking: bool,
    toggle: bool,
    key_stays_target: bool,
) -> io::Result<()> {
    let white_bold = "\x1b[1;37m";
    let rst = "\x1b[0m";

    if is_blinking {
        if toggle {
            write!(stdout, "{}\x1b[1m{} {}\x1b[1m{}{}", target_color, key, target_color, val_fmt, rst)
        } else {
            write!(stdout, "{}{} {}{}{}", white_bold, key, white_bold, val_fmt, rst)
        }
    } else {
        if key_stays_target {
            write!(stdout, "{}\x1b[1m{} {}\x1b[1m{}{}", target_color, key, target_color, val_fmt, rst)
        } else {
            write!(stdout, "{}{} {}\x1b[1m{}{}", white_bold, key, target_color, val_fmt, rst)
        }
    }
}

// --- System Info Queries ---

/// Reads sysfs boundaries identifying virtualbox/hypervisor emulation layers explicitly.
fn is_virtual_machine() -> bool {
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
        if cpuinfo.lines().any(|l| l.starts_with("flags") && l.contains("hypervisor")) { return true; }
    }
    if let Ok(prod) = fs::read_to_string("/sys/class/dmi/id/product_name") {
        let p = prod.to_lowercase();
        if p.contains("virtualbox") || p.contains("vmware") || p.contains("kvm") || p.contains("qemu") { return true; }
    }
    false
}

/// Pre-flight discovery engine to locate all absolute sysfs CPU frequency scaling endpoints.
/// Used to eliminate expensive `/proc/cpuinfo` string parsing during the active event loop.
fn discover_cpu_freq_paths() -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") {
        let mut cpus: Vec<usize> = entries.flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("cpu") {
                    name_str[3..].parse::<usize>().ok()
                } else {
                    None
                }
            })
            .collect();
        cpus.sort_unstable();
        for cpu in cpus {
            let path = format!("/sys/devices/system/cpu/cpu{}/cpufreq/scaling_cur_freq", cpu);
            if std::path::Path::new(&path).exists() {
                paths.push(path);
            }
        }
    }
    paths
}

/// Pre-flight discovery engine mapping absolute sysfs hwmon boundaries to static sensor structs.
/// Prevents recursive VFS directory traversals (`fs::read_dir`) during the intensive thermal polling loops.
fn discover_thermal_sensors() -> Vec<ThermalSensorDef> {
    let mut sensors = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = fs::read_to_string(path.join("name")).unwrap_or_default();
            let name_trim = name.trim();

            if !name_trim.contains("k10temp")
                && !name_trim.contains("coretemp")
                && !name_trim.contains("cpu_thermal")
                && !name_trim.contains("soc_thermal")
                && !name_trim.contains("pnv_thermal")
            {
                continue;
            }

            let parent_name = name_trim.to_string();

            if let Ok(files) = fs::read_dir(&path) {
                for file in files.flatten() {
                    let fname = file.file_name().to_string_lossy().into_owned();
                    if !fname.starts_with("temp") || !fname.ends_with("_input") { continue; }
                   
                    let input_path = file.path().to_string_lossy().into_owned();

                    let read_limit = |file_name: &str| {
                        fs::read_to_string(file.path().with_file_name(file_name))
                            .ok()
                            .and_then(|s| s.trim().parse::<f64>().ok())
                    };

                    let limit = read_limit(&fname.replace("_input", "_max"))
                        .or_else(|| read_limit(&fname.replace("_input", "_crit")))
                        .unwrap_or(95000.0);

                    let label = fs::read_to_string(path.join(fname.replace("_input", "_label")))
                        .unwrap_or_else(|_| fname.replace("_input", "")).trim().to_string();

                    let is_parent = label.contains("Tctl")
                        || label.contains("Package")
                        || label.contains("Tdie")
                        || label.contains("SoC")
                        || label.contains("cpu_thermal")
                        || label == "temp1";

                    sensors.push(ThermalSensorDef {
                        parent_name: parent_name.clone(),
                        label,
                        input_path,
                        limit,
                        is_parent,
                    });
                }
            }
        }
    }
    sensors
}

/// Renders perfect-width horizontal ASCII grid lines matching the active terminal footprint bounds dynamically.
fn get_dashed_line(max_w: usize, mid_text: &str) -> String {
    let padding = " ";
    let content_len = strip_ansi(mid_text) + padding.len() * 2;
    if content_len >= max_w { return "-".repeat(max_w); }
    let left_dashes = (max_w - content_len) / 2;
    let right_dashes = max_w - content_len - left_dashes;
    format!("{}{}{}{}{}", "-".repeat(left_dashes), padding, mid_text, padding, "-".repeat(right_dashes))
}

/// Discovers `temper-poll` executable binary path securely to prevent redundant shell-outs.
fn find_temper_poll() -> Option<std::path::PathBuf> {
    if let Ok(path) = which::which("temper-poll") { return Some(path); }
    let mut candidates = vec![
        std::path::PathBuf::from("/usr/local/bin/temper-poll"),
        std::path::PathBuf::from("/usr/bin/temper-poll"),
        std::path::PathBuf::from("/bin/temper-poll"),
        std::path::PathBuf::from("/opt/bin/temper-poll"),
    ];
    if let Ok(sudo_user) = std::env::var("SUDO_USER") { candidates.push(std::path::PathBuf::from(format!("/home/{}/.local/bin/temper-poll", sudo_user))); }
    if let Ok(home) = std::env::var("HOME") { candidates.push(std::path::PathBuf::from(home).join(".local/bin/temper-poll")); }
    for candidate in candidates { if candidate.is_file() { return Some(candidate); } }
    None
}

/// Reaches explicitly into desktop dbus session managers retrieving compositor session idle times.
/// Guarded by `DESKTOP_IDLE_DISABLED` to avoid CPU-heavy process creation loops when unavailable.
fn get_desktop_idle_time() -> Option<Duration> {
    if DESKTOP_IDLE_DISABLED.load(Ordering::Relaxed) {
        return None;
    }

    let run_cmd = |cmd: &str, args: &[&str]| {
        if let Ok(sudo_user) = std::env::var("SUDO_USER") {
            let script = format!("XDG_RUNTIME_DIR=/run/user/$(id -u {0}) DISPLAY=${{DISPLAY:-:0}} {1} {2}", sudo_user, cmd, args.join(" "));
            Command::new("sudo").args(["-u", &sudo_user, "sh", "-c", &script]).output()
        } else { Command::new(cmd).args(args).output() }
    };

    if let Ok(out) = run_cmd("busctl", &["--user", "call", "org.gnome.Mutter.IdleMonitor", "/org/gnome/Mutter/IdleMonitor/Core", "org.gnome.Mutter.IdleMonitor", "GetIdletime"]) {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(t_str) = s.split_whitespace().last() {
                if let Ok(ms) = t_str.parse::<u64>() { return Some(Duration::from_millis(ms)); }
            }
        }
    }

    if let Ok(out) = run_cmd("busctl", &["--user", "call", "org.kde.Screensaver", "/ScreenSaver", "org.kde.Screensaver", "GetSessionIdleTime"]) {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(t_str) = s.split_whitespace().last() {
                if let Ok(secs) = t_str.parse::<u32>() { return Some(Duration::from_secs(secs as u64)); }
            }
        }
    }

    if let Ok(out) = run_cmd("xprintidle", &[]) {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Ok(ms) = s.trim().parse::<u64>() { return Some(Duration::from_millis(ms)); }
        }
    }

    // Disable desktop DBus calls if unsupported to prevent background subprocess churn
    DESKTOP_IDLE_DISABLED.store(true, Ordering::Relaxed);
    None
}

/// Persistent tracker for fallback user activity monitoring avoiding repetitive directory traversals.
struct UserIdleTracker {
    paths: Vec<std::path::PathBuf>,
    last_input_mtime: SystemTime,
}

impl UserIdleTracker {
    /// Initializes the tracker by executing a full pre-flight discovery of input nodes.
    fn new() -> Self {
        let mut t = Self {
            paths: Vec::new(),
            last_input_mtime: SystemTime::UNIX_EPOCH,
        };
        t.paths = Self::discover_idle_paths();
        t
    }

    /// Recursively discovers all relevant active terminal and block input event endpoints.
    fn discover_idle_paths() -> Vec<std::path::PathBuf> {
        let mut paths = Vec::with_capacity(32);
        let mut check_dir = |dir: &str, prefix: &str| {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let fname = entry.file_name();
                    if prefix.is_empty() || fname.to_string_lossy().starts_with(prefix) {
                        paths.push(entry.path());
                    }
                }
            }
        };
        check_dir("/dev/input", "");
        check_dir("/dev/pts", "");
        check_dir("/dev", "tty");
        paths
    }

    /// Primary entry point for extracting system idle times.
    /// Defers to active Desktop DBUS compositors if available.
    /// Self-Heals its VFS cache if new input devices are hot-plugged `/dev/input` (mtime changes).
    fn get_user_idle_time(&mut self) -> Duration {
        if let Some(desktop_idle) = get_desktop_idle_time() {
            return desktop_idle;
        }

        // Self-heal: check if directory structure changed (e.g. USB mouse plugged in)
        if let Ok(meta) = fs::metadata("/dev/input") {
            if let Ok(mtime) = meta.modified() {
                if mtime > self.last_input_mtime {
                    self.last_input_mtime = mtime;
                    self.paths = Self::discover_idle_paths();
                }
            }
        }

        let mut newest_time = SystemTime::UNIX_EPOCH;
        let mut heal = false;

        for path in &self.paths {
            if let Ok(meta) = fs::metadata(path) {
                if let Ok(mtime) = meta.modified() {
                    if mtime > newest_time { newest_time = mtime; }
                }
            } else {
                heal = true;
            }
        }

        // Self-heal: a hardware device was disconnected. Refresh paths to stop throwing File IO errors.
        if heal {
            self.paths = Self::discover_idle_paths();
        }

        SystemTime::now().duration_since(newest_time).unwrap_or(Duration::ZERO)
    }
}

// --- Main Program Entry Point ---

/// Main entry point for CPU-Grid v4.0.0. Sets up crossbeam MPMC event channels, spawns telemetry & input threads, and enters the reactive event loop.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1).peekable();
    let mut cpu_interval = DEFAULT_INTERVAL;
    let mut room_interval = DEFAULT_INTERVAL;
    let mut mem_interval = DEFAULT_INTERVAL;
    let mut net_interval = DEFAULT_INTERVAL;
    let mut disk_interval = DEFAULT_INTERVAL;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => { print_help(); std::process::exit(0); }
            "-v" | "--version" => {
                println!("\x1b[1;38;2;255;215;0mCPU-Grid ver:{}\x1b[0m", env!("CARGO_PKG_VERSION"));
                println!("Copyright (C) 2026 StatusCode404 https://github.com/StatusCode404");
                std::process::exit(0);
            }
            "-n" | "--cpu-stats-interval" => { cpu_interval = args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL).clamp(0.1, 60.0); }
            "-r" | "--room-temp-interval" => { room_interval = args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL).clamp(1.0, 3600.0); }
            "-m" | "--mem-stats-interval" => { mem_interval = args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL).clamp(0.5, 60.0); }
            "-t" | "--net-interval" => { net_interval = args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL).clamp(0.5, 60.0); }
            "-d" | "--disk-interval" => { disk_interval = args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL).clamp(0.5, 60.0); }
            _ => {
                if arg.starts_with("-n") { cpu_interval = arg[2..].parse().unwrap_or(DEFAULT_INTERVAL).clamp(0.1, 60.0); }
                else if arg.starts_with("-r") { room_interval = arg[2..].parse().unwrap_or(DEFAULT_INTERVAL).clamp(1.0, 3600.0); }
                else if arg.starts_with("-m") { mem_interval = arg[2..].parse().unwrap_or(DEFAULT_INTERVAL).clamp(0.5, 60.0); }
                else if arg.starts_with("-t") { net_interval = arg[2..].parse().unwrap_or(DEFAULT_INTERVAL).clamp(0.5, 60.0); }
                else if arg.starts_with("-d") { disk_interval = arg[2..].parse().unwrap_or(DEFAULT_INTERVAL).clamp(0.5, 60.0); }
            }
        }
    }

    let is_vm = is_virtual_machine();
    let cpu_model_raw = fs::read_to_string("/proc/cpuinfo").unwrap_or_default().lines().find(|l| l.starts_with("model name") || l.starts_with("Processor") || l.starts_with("Hardware")).and_then(|l| l.split(':').nth(1)).map(|s| s.trim().to_string()).unwrap_or("Unknown".into());

    let cpu_model_display = cpu_model_raw
        .replace("AMD", "\x1b[1;38;2;237;28;36mAMD\x1b[0m\x1b[1m")
        .replace("Intel", "\x1b[1;38;2;0;113;197mIntel\x1b[0m\x1b[1m")
        .replace("Apple", "\x1b[1;38;2;192;192;192mApple\x1b[0m\x1b[1m")
        .replace("ARM", "\x1b[1;38;2;0;193;222mARM\x1b[0m\x1b[1m")
        .replace("RISC-V", "\x1b[1;38;2;155;81;224mRISC-V\x1b[0m\x1b[1m")
        .replace("IBM", "\x1b[1;38;2;31;112;193mIBM\x1b[0m\x1b[1m");

    let limits = (
        fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_min_freq").ok().and_then(|s| s.trim().parse::<f64>().ok()).map(|k| k / 1000.0),
        fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").ok().and_then(|s| s.trim().parse::<f64>().ok()).map(|k| k / 1000.0),
    );

    // Initializing MPMC Crossbeam Channel Pipeline
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = crossbeam_channel::unbounded();

    // 0. Background Input Event Reader Thread (MPMC Event Dispatcher)
    let tx_input = tx.clone();
    thread::Builder::new().name("cg-input".to_string()).spawn(move || loop {
        if let Ok(ev) = event::read() {
            if tx_input.send(Msg::InputEvent(ev)).is_err() {
                break;
            }
        }
    }).unwrap();

    // 0b. Background Strobe Ticker Thread (Fires 250ms visual ticks for 2Hz watermark strobes)
    let tx_tick = tx.clone();
    thread::Builder::new().name("cg-ticker".to_string()).spawn(move || loop {
        thread::sleep(Duration::from_millis(250));
        if tx_tick.send(Msg::Tick).is_err() {
            break;
        }
    }).unwrap();

    // 1. Background CPU Frequency Monitoring Thread
    // Employs a pre-flight discovery cache to bypass `/proc/cpuinfo` string parsing entirely unless sysfs is unavailable.
    let tx_cpu = tx.clone();
    thread::Builder::new().name("cg-cpu".to_string()).spawn(move || {
        let mut freq_paths = discover_cpu_freq_paths();
        let mut buf = String::with_capacity(8192);
        let mut freqs = Vec::with_capacity(128);
       
        loop {
            freqs.clear();
            let mut heal = false;
           
            if !freq_paths.is_empty() {
                for path in &freq_paths {
                    if let Ok(s) = fs::read_to_string(path) {
                        if let Ok(khz) = s.trim().parse::<f64>() {
                            freqs.push(khz / 1000.0);
                        } else {
                            heal = true; break;
                        }
                    } else {
                        heal = true; break;
                    }
                }
            } else {
                heal = true;
            }

            // Fallback to brute-force string parsing if sysfs paths are missing or dynamically unmapped, then attempt to self-heal.
            if heal {
                buf.clear();
                freqs.clear();
                if let Ok(mut file) = File::open("/proc/cpuinfo") { let _ = file.read_to_string(&mut buf); }
                for l in buf.lines() {
                    if l.starts_with("cpu MHz") || l.starts_with("BogoMIPS") {
                        if let Some(val_str) = l.split(':').nth(1) {
                            if let Ok(val) = val_str.trim().parse::<f64>() { freqs.push(val); }
                        }
                    }
                }
                freq_paths = discover_cpu_freq_paths();
            }

            if tx_cpu.send(Msg::CpuFreqs(freqs.clone())).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(cpu_interval));
        }
    }).unwrap();

    // 2. Background Thermal Monitoring Thread
    // Employs a pre-flight discovery cache to bypass VFS directory traversals. Traps `NotFound` errors and dynamically heals cache.
    let tx_ctemp = tx.clone();
    thread::Builder::new().name("cg-thermal".to_string()).spawn(move || {
        let mut sensors = discover_thermal_sensors();
       
        loop {
            let mut temps: HashMap<String, Vec<TempStat>> = HashMap::new();
            let mut heal = false;

            if sensors.is_empty() {
                heal = true;
            } else {
                for s in &sensors {
                    if let Ok(val_str) = fs::read_to_string(&s.input_path) {
                        if let Ok(val_milli) = val_str.trim().parse::<f64>() {
                            let val = val_milli / 1000.0;
                            let color = get_cpu_color(val_milli / s.limit);
                            temps.entry(s.parent_name.clone()).or_default().push(TempStat {
                                label: s.label.clone(),
                                val,
                                color,
                                is_parent: s.is_parent,
                            });
                        } else {
                            heal = true; break;
                        }
                    } else {
                        heal = true; break;
                    }
                }
            }

            // Self-Healing execution if cache reads fail or hardware dynamically re-assigns limits.
            if heal {
                sensors = discover_thermal_sensors();
               
                // If it remains genuinely empty (or just healed), pad output with safe default to prevent UI crashing.
                let default_val = if is_vm { TempStat { label: "VM".to_string(), val: 0.0, color: "\x1b[38;2;255;165;0m".to_string(), is_parent: true } }
                                  else { TempStat { label: "N/A".to_string(), val: 0.0, color: "\x1b[38;2;255;0;0m".to_string(), is_parent: true } };
                temps.insert("CPU Temps".to_string(), vec![default_val]);
            }

            let mut out = Vec::new();
            for (parent, mut chiplets) in temps {
                chiplets.sort_by(|a, b| b.is_parent.cmp(&a.is_parent).then(a.label.cmp(&b.label)));
                out.push((parent, chiplets));
            }

            if tx_ctemp.send(Msg::CpuTemps(out)).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(cpu_interval));
        }
    }).unwrap();

    // 3. Background USB Thermometer Monitoring Thread
    let tx_room = tx.clone();
    thread::Builder::new().name("cg-room".to_string()).spawn(move || {
        let cmd_path_opt = find_temper_poll();
        loop {
            let raw_temp = if let Some(ref cmd_path) = cmd_path_opt {
                let out = Command::new(cmd_path).output();
                if let Ok(o) = out {
                    let s = String::from_utf8_lossy(&o.stdout);
                    if let Some(line) = s.lines().find(|l| l.contains("Device #0:")) {
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if let Some(temp_str) = parts.iter().find(|p| p.contains('°')) {
                            let clean_temp = temp_str.replace('°', "").replace('C', "");
                            clean_temp.parse::<f64>().ok()
                        } else { None }
                    } else { None }
                } else { None }
            } else { None };

            if tx_room.send(Msg::RoomTemp(raw_temp)).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(room_interval));
        }
    }).unwrap();

    // 4. Background Memory Metrics Monitoring Thread
    // Optimized: Replaced expensive UTF-8 line string tokenization with SIMD-backed byte slice searching.
    let tx_mem = tx.clone();
    thread::Builder::new().name("cg-mem".to_string()).spawn(move || {
        let mut mem_buf = String::with_capacity(2048);
        let mut swap_buf = String::with_capacity(2048);
        let mut parsed_swaps = Vec::with_capacity(8);
        let mut swap_devices_formatted = Vec::with_capacity(8);

        loop {
            mem_buf.clear();
            swap_buf.clear();
            parsed_swaps.clear();
            swap_devices_formatted.clear();

            if let Ok(mut file) = File::open("/proc/meminfo") { let _ = file.read_to_string(&mut mem_buf); }
           
            // SIMD-backed zero-allocation byte search
            let parse_kb = |key: &str| -> u64 {
                if let Some(idx) = mem_buf.find(key) {
                    let rest = &mem_buf[idx + key.len()..];
                    if let Some(end) = rest.find("kB") {
                        return rest[..end].trim().parse::<u64>().unwrap_or(0);
                    }
                }
                0
            };

            let total = parse_kb("MemTotal:");
            let avail = parse_kb("MemAvailable:");
           
            let used_bytes = total.saturating_sub(avail) * 1024;
            let avail_bytes = avail * 1024;
            let total_bytes = total * 1024;
           
            let ram_percent = if total > 0 { (total - avail) as f64 / total as f64 } else { 0.0 };
            let mem_color = get_ram_color(ram_percent);

            let wb_used_parent = "\x1b[1;37mUsed\x1b[0m";
            let wb_avail_parent = "\x1b[1;37mAvail\x1b[0m";
            let wb_total_parent = "\x1b[1;37mTotal\x1b[0m";
            let cyan_bold = "\x1b[1;38;2;0;255;255m";

            let ram_str = format!("{col}\x1b[1m{}\x1b[0m {wb_used_parent} | {col}\x1b[1m{}\x1b[0m {wb_avail_parent} | {cya}{}\x1b[0m {wb_total_parent}",
                FormatSize(used_bytes), FormatSize(avail_bytes), FormatSize(total_bytes), col=mem_color, wb_used_parent=wb_used_parent, wb_avail_parent=wb_avail_parent, cya=cyan_bold);

            if let Ok(mut file) = File::open("/proc/swaps") { let _ = file.read_to_string(&mut swap_buf); }
            let mut total_swap = 0u64;
            let mut total_swap_used = 0u64;
           
            for line in swap_buf.lines().skip(1) {
                let p: Vec<&str> = line.split_whitespace().collect();
                if p.len() >= 4 {
                    let size_bytes = p[2].parse::<u64>().unwrap_or(0) * 1024;
                    let used_bytes = p[3].parse::<u64>().unwrap_or(0) * 1024;
                    total_swap += size_bytes;
                    total_swap_used += used_bytes;
                    parsed_swaps.push((p[0].split('/').last().unwrap_or("swap").to_string(), used_bytes, size_bytes));
                }
            }

            let max_swap_len = parsed_swaps.iter().map(|(n, _, _)| n.len()).max().unwrap_or(4);
           
            for (name, used_bytes, size_bytes) in &parsed_swaps {
                let swap_percent = if *size_bytes > 0 { *used_bytes as f64 / *size_bytes as f64 } else { 0.0 };
                let col = get_swap_color(swap_percent);
                swap_devices_formatted.push(format!(" {:<width$}: {col}{}\x1b[0m \x1b[0;37mUsed\x1b[0m / \x1b[38;2;0;255;255m{}\x1b[0m \x1b[0;37mTotal\x1b[0m",
                    name, FormatSize(*used_bytes), FormatSize(*size_bytes), col=col, width=max_swap_len));
            }

            let total_swap_percent = if total_swap > 0 { total_swap_used as f64 / total_swap as f64 } else { 0.0 };
            let swap_col = get_swap_color(total_swap_percent);
            let swap_total_str = format!("{col}\x1b[1m{}\x1b[0m {wb_used_parent} | {cya}\x1b[1m{}\x1b[0m {wb_total_parent} | {col}\x1b[1m{:.1}%\x1b[0m \x1b[0;37m%Used\x1b[0m",
                FormatSize(total_swap_used), FormatSize(total_swap), total_swap_percent * 100.0, col=swap_col, wb_used_parent=wb_used_parent, wb_total_parent=wb_total_parent, cya=cyan_bold);

            let zswap_param_path = std::path::Path::new("/sys/module/zswap/parameters/enabled");
            let zswap_str = if !zswap_param_path.exists() { "\x1b[38;2;238;130;238m\x1b[1mNot Present\x1b[0m".to_string() }
            else {
                match fs::read_to_string(zswap_param_path) {
                    Ok(val) => match val.trim() {
                        "Y" => {
                            match (fs::read_to_string("/sys/kernel/debug/zswap/pool_total_size"), fs::read_to_string("/sys/kernel/debug/zswap/stored_pages")) {
                                (Ok(p_str), Ok(pg_str)) => {
                                    let pool_bytes = p_str.trim().parse::<u64>().unwrap_or(0);
                                    let pages = pg_str.trim().parse::<u64>().unwrap_or(0);
                                    let ratio = if pool_bytes > 0 { (pages * 4 * 1024) as f64 / (pool_bytes as f64) } else { 0.0 };
                                    let pool_color = if pool_bytes > 0 { "\x1b[38;2;0;200;0m" } else { "\x1b[38;2;150;150;150m" };
                                    let ratio_color = if ratio > 0.0 { get_ratio_color(ratio) } else { "\x1b[0m".to_string() };
                                    let algo = fs::read_to_string("/sys/module/zswap/parameters/compressor").unwrap_or_else(|_| "unknown".to_string());
                                    let algo_trim = algo.trim();
                                    let algo_color = get_zswap_algo_color(algo_trim);
                                    format!("\x1b[38;2;0;200;0m\x1b[1mEnabled\x1b[0m | \x1b[1mAlgo:\x1b[0m {algo_color}\x1b[1m{algo_trim}\x1b[0m | \x1b[1mPool:\x1b[0m {pool_color}\x1b[1m{}\x1b[0m | \x1b[1mRatio:\x1b[0m {ratio_color}\x1b[1m{:.1}:1\x1b[0m", FormatSize(pool_bytes), ratio)
                                }
                                _ => "\x1b[38;2;0;200;0m\x1b[1mEnabled\x1b[0m (\x1b[38;2;255;255;0m\x1b[1mRequires sudo for detailed stats\x1b[0m)".to_string(),
                            }
                        }
                        "N" => "\x1b[38;2;255;0;0m\x1b[1mDisabled\x1b[0m".to_string(),
                        _ => "\x1b[38;2;255;0;0m\x1b[1mUnknown\x1b[0m".to_string(),
                    },
                    Err(_) => "\x1b[38;2;255;0;0m\x1b[1mUnknown\x1b[0m".to_string(),
                }
            };

            if tx_mem.send(Msg::MemStats { ram_str, zswap_str, swap_total_str, swaps_formatted: swap_devices_formatted.clone() }).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(mem_interval));
        }
    }).unwrap();

    // 5. Background Network Interface Traffic Thread
    // Optimized: Max line speeds are dynamically mapped/cached to eliminate repeated file I/O operations.
    let tx_net = tx.clone();
    thread::Builder::new().name("cg-net".to_string()).spawn(move || {
        let mut prev_stats: HashMap<String, (u64, u64, Instant)> = HashMap::with_capacity(16);
        let mut max_speeds: HashMap<String, f64> = HashMap::with_capacity(16);
        let mut current_stats = Vec::with_capacity(8);
        let mut current_keys = HashSet::with_capacity(8);
        let mut dev_str = String::with_capacity(4096);
       
        loop {
            current_stats.clear();
            current_keys.clear();
            dev_str.clear();
            let now = Instant::now();

            if let Ok(mut file) = File::open("/proc/net/dev") { let _ = file.read_to_string(&mut dev_str); }
            for line in dev_str.lines().skip(2) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() < 17 { continue; }
                let iface = parts[0].trim_end_matches(':').to_string();
                if iface == "lo" { continue; }

                let rx = parts[1].parse::<u64>().unwrap_or(0);
                let tx = parts[9].parse::<u64>().unwrap_or(0);
                current_keys.insert(iface.clone());

                let max_bytes_per_sec = *max_speeds.entry(iface.clone()).or_insert_with(|| {
                    let speed_mbps = fs::read_to_string(format!("/sys/class/net/{}/speed", iface))
                        .ok()
                        .and_then(|s| s.trim().parse::<f64>().ok())
                        .unwrap_or(1000.0);
                    speed_mbps * 1_000_000.0 / 8.0
                });

                let (rx_speed, tx_speed) = if let Some(&(prev_rx, prev_tx, prev_time)) = prev_stats.get(&iface) {
                    let duration = now.duration_since(prev_time).as_secs_f64();
                    if duration > 0.0 { (rx.saturating_sub(prev_rx) as f64 / duration, tx.saturating_sub(prev_tx) as f64 / duration) } else { (0.0, 0.0) }
                } else {
                    let _ = tx_net.send(Msg::NetEvent(iface.clone(), "ACTIVATED".to_string()));
                    (0.0, 0.0)
                };

                current_stats.push((iface.clone(), rx_speed, tx_speed, max_bytes_per_sec));
                prev_stats.insert(iface, (rx, tx, now));
            }

            prev_stats.retain(|k, _| {
                let keep = current_keys.contains(k);
                if !keep {
                    let _ = tx_net.send(Msg::NetEvent(k.clone(), "DEACTIVATED".to_string()));
                    max_speeds.remove(k); // Flush interface cache if device goes offline
                }
                keep
            });

            if tx_net.send(Msg::NetStats(current_stats.clone())).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(net_interval));
        }
    }).unwrap();

    // 6. Background User Activity Tracker Thread
    // Optimized: Replaced brute directory traversal per-second with a pre-mapped cached node struct.
    let tx_idle = tx.clone();
    thread::Builder::new().name("cg-idle".to_string()).spawn(move || {
        let mut tracker = UserIdleTracker::new();
        loop {
            if tx_idle.send(Msg::UserIdle(tracker.get_user_idle_time())).is_err() { break; }
            thread::sleep(Duration::from_secs(1));
        }
    }).unwrap();

    // 7. Background Storage Telemetry Thread
    // Optimized: Caches `/proc/mounts` layout strings strictly once every 10 seconds.
    let tx_disk = tx.clone();
    thread::Builder::new().name("cg-disk".to_string()).spawn(move || {
        struct MountCache { dev: String, mount: String, fs_type: String, c_path: std::ffi::CString }
        let allowed_fs: HashSet<&str> = ["ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "vfat", "exfat", "ntfs", "ntfs3", "f2fs"].into_iter().collect();

        let mut prev_disk_stats: HashMap<String, (u64, u64, Instant)> = HashMap::with_capacity(16);
        let mut mount_caps: HashMap<u64, (u64, u64, String, String, String)> = HashMap::with_capacity(16);
        let mut dev_traffic: HashMap<String, (f64, f64)> = HashMap::with_capacity(16);
        let mut disk_nodes = Vec::with_capacity(16);
        let mut active_devs = HashSet::with_capacity(16);
        let mut mounts_str = String::with_capacity(4096);
        let mut diskstats_str = String::with_capacity(8192);

        let mut mount_cache: Vec<MountCache> = Vec::with_capacity(16);
        let mut last_mount_check = Instant::now() - Duration::from_secs(100);

        loop {
            dev_traffic.clear();
            disk_nodes.clear();
            active_devs.clear();
            diskstats_str.clear();

            if last_mount_check.elapsed().as_secs() >= 10 {
                mount_cache.clear();
                mounts_str.clear();
                if let Ok(mut f) = File::open("/proc/mounts") { let _ = f.read_to_string(&mut mounts_str); }
                for line in mounts_str.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() < 6 { continue; }
                    let dev = parts[0];
                    let mount = parts[1];
                    let fs_type = parts[2];
                   
                    if allowed_fs.contains(fs_type) {
                        if let Ok(c_path) = std::ffi::CString::new(mount) {
                            mount_cache.push(MountCache {
                                dev: dev.to_string(),
                                mount: mount.to_string(),
                                fs_type: fs_type.to_string(),
                                c_path,
                            });
                        }
                    }
                }
                last_mount_check = Instant::now();
            }

            mount_caps.clear();
            for mc in &mount_cache {
                let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
                if unsafe { libc::statvfs(mc.c_path.as_ptr(), &mut stat) } == 0 {
                    let total_bytes = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
                    let free_bytes = (stat.f_bfree as u64).saturating_mul(stat.f_frsize as u64);
                    let used_bytes = total_bytes.saturating_sub(free_bytes);
                    let fsid = stat.f_fsid as u64;

                    if let Some(existing) = mount_caps.get_mut(&fsid) {
                        if mc.mount.len() < existing.2.len() {
                            existing.2 = mc.mount.clone();
                        }
                    } else {
                        mount_caps.insert(fsid, (total_bytes, used_bytes, mc.mount.clone(), mc.fs_type.clone(), mc.dev.clone()));
                    }
                }
            }

            let now = Instant::now();
            if let Ok(mut f) = File::open("/proc/diskstats") { let _ = f.read_to_string(&mut diskstats_str); }
            for line in diskstats_str.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 14 {
                    let dev_name = parts[2];
                    let sectors_read = parts[5].parse::<u64>().unwrap_or(0);
                    let sectors_written = parts[9].parse::<u64>().unwrap_or(0);
                   
                    let bytes_read = sectors_read * 512;
                    let bytes_written = sectors_written * 512;

                    let (r_speed, w_speed) = if let Some(&(p_read, p_write, p_time)) = prev_disk_stats.get(dev_name) {
                        let duration = now.duration_since(p_time).as_secs_f64();
                        if duration > 0.0 {
                            (bytes_read.saturating_sub(p_read) as f64 / duration, bytes_written.saturating_sub(p_write) as f64 / duration)
                        } else { (0.0, 0.0) }
                    } else { (0.0, 0.0) };

                    dev_traffic.insert(dev_name.to_string(), (r_speed, w_speed));
                    prev_disk_stats.insert(dev_name.to_string(), (bytes_read, bytes_written, now));
                }
            }

            let mut agg_total = 0u64;
            let mut agg_used = 0u64;

            let mut sorted_mounts: Vec<_> = mount_caps.values().collect();
            sorted_mounts.sort_by(|a, b| a.2.cmp(&b.2));

            let max_mount_len = sorted_mounts.iter().map(|t| t.2.chars().count()).max().unwrap_or(4);

            for tuple in &sorted_mounts {
                let total_bytes: u64 = (*tuple).0;
                let used_bytes: u64 = (*tuple).1;
                let mount: &String = &(*tuple).2;
                let fs_type: &String = &(*tuple).3;
                let dev: &String = &(*tuple).4;

                if total_bytes == 0 { continue; }
                agg_total += total_bytes;
                agg_used += used_bytes;

                let raw_dev_name = if let Ok(path) = fs::canonicalize(dev) {
                    path.file_name().unwrap_or_default().to_string_lossy().to_string()
                } else {
                    dev.split('/').last().unwrap_or("").to_string()
                };

                active_devs.insert(raw_dev_name.clone());

                let (r_speed, w_speed) = dev_traffic.get(&raw_dev_name).unwrap_or(&(0.0, 0.0));
               
                let percent = used_bytes as f64 / total_bytes as f64;
                let is_cow = fs_type.contains("btrfs") || fs_type.contains("zfs");
                let capacity_color = get_disk_capacity_color(percent, is_cow);

                let mount_padded = format!(" {:<width$}: ", mount, width = max_mount_len);
               
                disk_nodes.push(DiskNodeRaw {
                    mount_padded,
                    used_bytes,
                    total_bytes,
                    capacity_color,
                    r_speed: *r_speed,
                    w_speed: *w_speed,
                    raw_dev_name,
                });
            }

            prev_disk_stats.retain(|k, _| active_devs.contains(k));

            if tx_disk.send(Msg::DiskStats {
                disk_nodes: disk_nodes.clone(),
                agg_total,
                agg_used,
            }).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(disk_interval));
        }
    }).unwrap();

    // --- Terminal Raw Mode & Canvas Setup ---
    terminal::enable_raw_mode()?;
    let mut stdout = BufWriter::new(io::stdout());
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    stdout.queue(terminal::Clear(ClearType::All))?;

    // Cache initial terminal size to prevent 20 Hz kernel ioctl calls
    let (mut max_w, mut max_h) = terminal::size().unwrap_or((80, 24));

    let mut state = SystemState {
        cpu_model_display,
        limits,
        highest_overclock: None,
        hoc_last_peak: None,
        freqs: vec![],
        cpu_temps: vec![],
        parent_ht_trackers: HashMap::new(),
        room_temp_val: None,
        room_temp_tracker: ValuePeakTracker::new(),
        last_room_hot_time: None,
        mem_ram_str: "Loading...".into(),
        mem_zswap_str: "Loading...".into(),
        mem_swap_total_str: "Loading...".into(),
        swaps_formatted: vec![],
        net_events: HashMap::new(),
       
        raw_net_nodes: Vec::with_capacity(8),
        net_trackers: HashMap::new(),
        net_total_tracker: (ValuePeakTracker::new(), ValuePeakTracker::new()),
        net_total_rx: 0.0,
        net_total_tx: 0.0,
        net_total_max: 0.0,
       
        disk_nodes: Vec::with_capacity(16),
        disk_trackers: HashMap::with_capacity(16),
        disk_global_hw: StorageHrHwState::new(),
        disk_agg_total: 0,
        disk_agg_used: 0,
        disk_agg_read: 0.0,
        disk_agg_write: 0.0,
       
        idle_time: Duration::ZERO,
       
        net_total_str: String::with_capacity(256),
        display_room_temp: String::with_capacity(256),
        net_display_pool: Vec::with_capacity(16),
        disk_cap_parent: String::with_capacity(256),
        disk_io_parent: String::with_capacity(256),
        disk_combined_parent: String::with_capacity(512),
        disk_cap_pool: Vec::with_capacity(16),
        disk_io_pool: Vec::with_capacity(16),
        disk_combined_pool: Vec::with_capacity(16),
        disk_io_split_pool: Vec::with_capacity(16),

        screen_is_dirty: true,
    };

    // --- MPMC Event-Driven Reactive Render Loop ---
    'mainloop: loop {
        // Block until first channel event arrives (Dormant at 0.00% CPU when idle)
        let first_msg = match rx.recv() {
            Ok(m) => m,
            Err(_) => break 'mainloop,
        };

        // Process all queued channels instantly
        let mut msg_opt = Some(first_msg);
        while let Some(msg) = msg_opt {
            match msg {
                Msg::InputEvent(ev) => match ev {
                    Event::Key(key) => {
                        if key.code == KeyCode::Char('q') || key.code == KeyCode::Char('Q') || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
                            break 'mainloop;
                        }
                    }
                    Event::Resize(w, h) => {
                        max_w = w;
                        max_h = h;
                        state.screen_is_dirty = true;
                    }
                    _ => {}
                },
                Msg::Tick => {
                    if is_any_blink_active(&state) {
                        state.screen_is_dirty = true;
                    }
                }
                Msg::CpuFreqs(f) => {
                    if let Some(max_limit) = state.limits.1 {
                        for &freq in &f {
                            if freq > max_limit {
                                if let Some(current_hoc) = state.highest_overclock {
                                    if freq > current_hoc {
                                        state.highest_overclock = Some(freq);
                                        state.hoc_last_peak = Some(Instant::now());
                                    }
                                } else {
                                    state.highest_overclock = Some(freq);
                                    state.hoc_last_peak = Some(Instant::now());
                                }
                            }
                        }
                    }
                    state.freqs = f;
                    state.screen_is_dirty = true;
                }
                Msg::CpuTemps(t) => {
                    for (parent_name, chiplets) in &t {
                        for stat in chiplets {
                            if stat.is_parent {
                                let tracker = state.parent_ht_trackers.entry(parent_name.clone()).or_insert_with(ValuePeakTracker::new);
                                tracker.update_max(stat.val, &stat.color);
                            }
                        }
                    }
                    state.cpu_temps = t;
                    state.screen_is_dirty = true;
                }
                Msg::RoomTemp(r) => {
                    state.room_temp_val = r;
                    if let Some(val) = r {
                        let color = get_room_temp_color(val);
                        state.room_temp_tracker.update_max(val, &color);
                        state.room_temp_tracker.update_min(val, &color);
                       
                        if val >= 40.0 {
                            state.last_room_hot_time = Some(Instant::now());
                        }
                    }
                    state.screen_is_dirty = true;
                }
                Msg::MemStats { ram_str, zswap_str, swap_total_str, swaps_formatted } => {
                    state.mem_ram_str = ram_str;
                    state.mem_zswap_str = zswap_str;
                    state.mem_swap_total_str = swap_total_str;
                    state.swaps_formatted = swaps_formatted;
                    state.screen_is_dirty = true;
                }
                Msg::NetStats(n) => {
                    state.net_total_rx = 0.0;
                    state.net_total_tx = 0.0;
                    state.net_total_max = 0.0;
                   
                    for (iface, rx_speed, tx_speed, max_bytes) in &n {
                        state.net_total_rx += rx_speed;
                        state.net_total_tx += tx_speed;
                        state.net_total_max += max_bytes;

                        let rx_col = get_net_color(*rx_speed, *max_bytes);
                        let tx_col = get_net_color(*tx_speed, *max_bytes);

                        let trackers = state.net_trackers.entry(iface.clone()).or_insert_with(|| (ValuePeakTracker::new(), ValuePeakTracker::new()));
                        trackers.0.update_max(*rx_speed, &rx_col);
                        trackers.1.update_max(*tx_speed, &tx_col);
                    }
                   
                    let total_rx_col = get_net_color(state.net_total_rx, state.net_total_max.max(1.0));
                    let total_tx_col = get_net_color(state.net_total_tx, state.net_total_max.max(1.0));

                    state.net_total_tracker.0.update_max(state.net_total_rx, &total_rx_col);
                    state.net_total_tracker.1.update_max(state.net_total_tx, &total_tx_col);
                    state.raw_net_nodes = n;
                    state.screen_is_dirty = true;
                },
                Msg::DiskStats { disk_nodes, agg_total, agg_used } => {
                    state.disk_agg_read = 0.0;
                    state.disk_agg_write = 0.0;
                   
                    for node in &disk_nodes {
                        let hw = state.disk_trackers.entry(node.raw_dev_name.clone()).or_insert_with(StorageHrHwState::new);
                        hw.update(node.r_speed, node.w_speed);
                        state.disk_agg_read += node.r_speed;
                        state.disk_agg_write += node.w_speed;
                    }
                   
                    state.disk_global_hw.update(state.disk_agg_read, state.disk_agg_write);
                    state.disk_nodes = disk_nodes;
                    state.disk_agg_total = agg_total;
                    state.disk_agg_used = agg_used;
                    state.screen_is_dirty = true;
                },
                Msg::NetEvent(iface, event_str) => {
                    state.net_events.insert(iface, (event_str, Instant::now()));
                    state.screen_is_dirty = true;
                },
                Msg::UserIdle(d) => {
                    state.idle_time = d;
                    state.screen_is_dirty = true;
                },
            }
            msg_opt = rx.try_recv().ok();
        }

        // Skip TUI frame draw if screen buffer state is not dirty
        if !state.screen_is_dirty {
            continue;
        }

        state.net_events.retain(|_, (_, time)| time.elapsed().as_secs() < 5);
        state.disk_trackers.retain(|k, _| state.disk_nodes.iter().any(|n| &n.raw_dev_name == k));

        if max_w < MIN_CELL_WIDTH as u16 {
            stdout.queue(terminal::Clear(ClearType::All))?;
            stdout.queue(cursor::MoveTo(0, 0))?;
            write!(stdout, "Terminal too small!")?;
            stdout.flush()?;
            state.screen_is_dirty = false;
            continue;
        }

        stdout.queue(cursor::MoveTo(0, 0))?;
        let mut row = 0;

        let print_line = |row: &mut u16, text: &str, stdout: &mut BufWriter<io::Stdout>| -> io::Result<()> {
            if *row < max_h {
                stdout.queue(cursor::MoveTo(0, *row))?;
                write!(stdout, "{}", text)?;
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
                *row += 1;
            }
            Ok(())
        };

        let print_aligned_2col_grid = |row: &mut u16, items: &[String], max_w: usize, stdout: &mut BufWriter<io::Stdout>| -> io::Result<()> {
            let mut col1_w = 0;
            let mut check_idx = 0;
            while check_idx < items.len() {
                let len = strip_ansi(&items[check_idx]);
                if len > col1_w { col1_w = len; }
                check_idx += 2;
            }

            let mut i = 0;
            while i < items.len() {
                let item1 = &items[i];
                let len1 = strip_ansi(item1);

                if i + 1 < items.len() {
                    let item2 = &items[i+1];
                    let len2 = strip_ansi(item2);

                    if col1_w + 3 + len2 <= max_w {
                        let pad = " ".repeat(col1_w.saturating_sub(len1));
                        print_line(row, &format!("{}{} | {}", item1, pad, item2), stdout)?;
                        i += 2;
                        continue;
                    }
                }
                print_line(row, item1, stdout)?;
                i += 1;
            }
            Ok(())
        };

        let toggle = get_blink_toggle();

        // --- Render Header & Core Limits ---
        let version = env!("CARGO_PKG_VERSION");
        print_line(&mut row, &format!("\x1b[1;38;2;255;215;0mCPU-Grid ver:{}\x1b[0m", version), &mut stdout)?;
        print_line(&mut row, &format!("\x1b[1m{}\x1b[0m", state.cpu_model_display), &mut stdout)?;
        print_line(&mut row, "\x1b[1;35mType 'Q' or Ctrl+C to quit.\x1b[0m", &mut stdout)?;

        let limits_str = match state.limits {
            (Some(min), Some(max)) => format!("\x1b[1mHardware Limits:\x1b[0m \x1b[1;38;2;100;255;100m{:.0}\x1b[0m MHz Min | \x1b[1;38;2;255;0;0m{:.0}\x1b[0m MHz Max", min, max),
            _ => {
                if is_vm { "\x1b[1mHardware Limits:\x1b[0m \x1b[38;2;255;165;0mVM detected, limits not exposed\x1b[0m".to_string() }
                else { "\x1b[1mHardware Limits:\x1b[0m \x1b[38;2;255;0;0mUnavailable\x1b[0m".to_string() }
            }
        };

        if let Some(hoc) = state.highest_overclock {
            if row < max_h {
                stdout.queue(cursor::MoveTo(0, row))?;
                write!(stdout, "{} | ", limits_str)?;
                write_watermark_inline(
                    &mut stdout,
                    "HOC:",
                    &FormatHoc(hoc),
                    &"\x1b[1;38;2;238;130;238m",
                    is_blink_active(state.hoc_last_peak),
                    toggle,
                    true
                )?;
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
                row += 1;
            }
        } else {
            print_line(&mut row, &limits_str, &mut stdout)?;
        }

        let idle_secs = state.idle_time.as_secs();
        if idle_secs < 5 {
            print_line(&mut row, "\x1b[1mUser Activity:\x1b[0m \x1b[1m\x1b[38;2;0;255;255mACTIVE\x1b[0m", &mut stdout)?;
        } else {
            print_line(&mut row, &format!("\x1b[1mUser Activity:\x1b[0m \x1b[1m{}IDLE {}\x1b[0m", get_idle_color(idle_secs), FormatIdleTime(idle_secs)), &mut stdout)?;
        }

        // --- Render Temperatures Heat-Map ---
        if row < max_h {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, "\x1b[1;38;2;255;215;0mTemperature Heat-Map\x1b[0m"))?;
            row += 1;
        }

        if let Some(temp) = state.room_temp_val {
            state.display_room_temp.clear();
            let _ = write!(&mut state.display_room_temp, "{}\x1b[1m{:.1}°C\x1b[0m | ", get_room_temp_color(temp), temp);
           
            write_watermark_buf(
                &mut state.display_room_temp,
                "LRT:",
                &format!("{:.1}°C", state.room_temp_tracker.min_val),
                &state.room_temp_tracker.min_color,
                is_blink_active(state.room_temp_tracker.last_min_peak),
                toggle,
                false
            );
           
            let _ = write!(&mut state.display_room_temp, " | ");
           
            write_watermark_buf(
                &mut state.display_room_temp,
                "HRT:",
                &format!("{:.1}°C", state.room_temp_tracker.max_val),
                &state.room_temp_tracker.max_color,
                is_blink_active(state.room_temp_tracker.last_max_peak),
                toggle,
                false
            );

            let is_warning_active = if let Some(last_hot) = state.last_room_hot_time { last_hot.elapsed().as_secs() < 300 } else { false };
            if is_warning_active {
                let warn_tag = if toggle { "\x1b[1;38;2;255;165;0mWARNING:\x1b[0m" } else { "\x1b[38;2;255;165;0mWARNING:\x1b[0m" };
                let _ = write!(&mut state.display_room_temp, " {warn_tag} \x1b[1;38;2;238;130;238mAmbient Temp is too HOT! Consider Shutting Down!\x1b[0m");
            }

            print_line(&mut row, &format!("\x1b[1mRoom Temp:\x1b[0m {}", state.display_room_temp), &mut stdout)?;
        } else {
            print_line(&mut row, "\x1b[1mRoom Temp:\x1b[0m \x1b[38;2;255;0;0mNo thermometer detected\x1b[0m", &mut stdout)?;
        }

        for (parent, chiplets) in &state.cpu_temps {
            if chiplets.is_empty() { continue; }
            print_line(&mut row, &format!("\x1b[1mCPU Temps ({}):\x1b[0m", parent), &mut stdout)?;
           
            let cols = (max_w as usize / MIN_CELL_WIDTH).max(1).min(chiplets.len().max(1));
            let temp_rows = (chiplets.len() + cols - 1) / cols;
            let max_lbl = chiplets.iter().map(|c| c.label.len()).max().unwrap_or(4).max(4);

            let (ht_val, ht_last_peak, ht_max_color) = match state.parent_ht_trackers.get(parent) {
                Some(t) => (t.max_val, t.last_max_peak, t.max_color.clone()),
                None => (0.0, None, "\x1b[1;37m".to_string()),
            };

            for r in 0..temp_rows {
                if row >= max_h { break; }
                stdout.queue(cursor::MoveTo(0, row))?;
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
                for c in 0..cols {
                    let idx = r * cols + c;
                    if idx < chiplets.len() {
                        let stat = &chiplets[idx];
                        let sep = if c < cols - 1 { " | " } else { "" };
                        let fw = if stat.is_parent { "\x1b[1m" } else { "" };
                       
                        let _ = write!(stdout, "{}{:>width$}: {}{:4.1}°C\x1b[0m", fw, stat.label, stat.color, stat.val, width=max_lbl);
                        if stat.is_parent {
                            let _ = write!(stdout, " | ");
                            let _ = write_watermark_inline(
                                &mut stdout,
                                "HT:",
                                &format!("{:4.1}°C", ht_val),
                                &ht_max_color,
                                is_blink_active(ht_last_peak),
                                toggle,
                                false
                            );
                        }
                        let _ = write!(stdout, "{}", sep);
                    }
                }
                row += 1;
            }
        }

        // --- Render Core Heat-Map ---
        let core_msg = if state.freqs.is_empty() { "Error: Core Frequencies cannot be accessed" }
                       else if is_vm { "VM Guest Detected" }
                       else { "Core Heat-Map" };

        if row < max_h {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, &format!("\x1b[1;38;2;255;215;0m{}\x1b[0m", core_msg)))?;
            row += 1;
        }

        let cols = (max_w as usize / MIN_CELL_WIDTH).max(1).min(state.freqs.len().max(1));
        let cpu_rows = (state.freqs.len() + cols - 1) / cols;

        for r in 0..cpu_rows {
            if row >= max_h { break; }
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            for c in 0..cols {
                let idx = r * cols + c;
                if idx < state.freqs.len() {
                    let freq = state.freqs[idx];
                    let color = if let (Some(min), Some(max)) = state.limits { get_cpu_color((freq - min) / (max - min)) } else { String::new() };
                    let (display_freq, unit) = if freq >= 1_000_000.0 { (freq / 1_000_000.0, "THz") } else if freq >= 1000.0 { (freq / 1000.0, "GHz") } else { (freq, "MHz") };
                    let sep = if c < cols - 1 { " | " } else { "" };
                   
                    let _ = write!(stdout, "C{:02}: {}{} {}\x1b[0m{}", idx, color, FormatDynamic6(display_freq), unit, sep);
                }
            }
            row += 1;
        }

        // --- Render Memory Heat-Map ---
        if row < max_h {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, "\x1b[1;38;2;255;215;0mMemory Heat-Map\x1b[0m"))?;
            row += 1;
        }

        print_line(&mut row, &format!("\x1b[1mRAM:\x1b[0m {}", state.mem_ram_str), &mut stdout)?;
        print_line(&mut row, &format!("\x1b[1mZswap:\x1b[0m {}", state.mem_zswap_str), &mut stdout)?;
        print_line(&mut row, &format!("\x1b[1mSwap:\x1b[0m {}", state.mem_swap_total_str), &mut stdout)?;
        print_aligned_2col_grid(&mut row, &state.swaps_formatted, max_w as usize, &mut stdout)?;

        // --- Render Network Heat-Map ---
        if row < max_h {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, "\x1b[1;38;2;255;215;0mNetwork Heat-Map\x1b[0m"))?;
            row += 1;
        }

        let align_len = 9.max(state.raw_net_nodes.iter().map(|(iface, _, _, _)| iface.len()).max().unwrap_or(0));

        while state.net_display_pool.len() < state.raw_net_nodes.len() + state.net_events.len() {
            state.net_display_pool.push(String::with_capacity(256));
        }

        let mut net_idx = 0;

        for (iface, rx, tx, max) in state.raw_net_nodes.iter() {
            let buf = &mut state.net_display_pool[net_idx];
            buf.clear();

            if let Some((ev, time)) = state.net_events.get(iface) {
                if time.elapsed().as_secs() < 5 {
                    let ev_color = if ev == "ACTIVATED" { "\x1b[38;2;0;200;0m" } else { "\x1b[38;2;255;255;0m" };
                    let _ = write!(buf, "{:>width$}: \x1b[1m{}{}{}\x1b[0m", iface, ev_color, ev, "\x1b[0m", width=align_len);
                    net_idx += 1;
                    continue;
                }
            }

            let rx_col = get_net_color(*rx, *max);
            let tx_col = get_net_color(*tx, *max);
            let trackers = state.net_trackers.get(iface).unwrap();

            let _ = write!(buf, "{:>width$}: {}\x1b[1m{}\x1b[0m \x1b[1;37m↓\x1b[0m  {}\x1b[1m{}\x1b[0m \x1b[1;37m↑\x1b[0m  ",
                iface, rx_col, FormatNetSpeed(*rx), tx_col, FormatNetSpeed(*tx), width=align_len);
           
            write_watermark_buf(buf, "H↓:", &FormatNetSpeed(trackers.0.max_val), &trackers.0.max_color, is_blink_active(trackers.0.last_max_peak), toggle, false);
            let _ = write!(buf, "  ");
            write_watermark_buf(buf, "H↑:", &FormatNetSpeed(trackers.1.max_val), &trackers.1.max_color, is_blink_active(trackers.1.last_max_peak), toggle, false);
           
            net_idx += 1;
        }

        for (iface, (ev, time)) in &state.net_events {
            if ev == "DEACTIVATED" && time.elapsed().as_secs() < 5 {
                let buf = &mut state.net_display_pool[net_idx];
                buf.clear();
                let _ = write!(buf, "{:>width$}: \x1b[38;2;255;255;0m\x1b[1mDEACTIVATED\x1b[0m", iface, width=align_len);
                net_idx += 1;
            }
        }

        state.net_total_str.clear();
        let total_rx_col = get_net_color(state.net_total_rx, state.net_total_max.max(1.0));
        let total_tx_col = get_net_color(state.net_total_tx, state.net_total_max.max(1.0));
       
        let _ = write!(&mut state.net_total_str, "\x1b[1m{:>width$}:\x1b[0m {}\x1b[1m{}\x1b[0m \x1b[1;37m↓\x1b[0m  {}\x1b[1m{}\x1b[0m \x1b[1;37m↑\x1b[0m  ",
            "Net Total", total_rx_col, FormatNetSpeed(state.net_total_rx), total_tx_col, FormatNetSpeed(state.net_total_tx), width=align_len);

        write_watermark_buf(&mut state.net_total_str, "H↓:", &FormatNetSpeed(state.net_total_tracker.0.max_val), &state.net_total_tracker.0.max_color, is_blink_active(state.net_total_tracker.0.last_max_peak), toggle, false);
        let _ = write!(&mut state.net_total_str, "  ");
        write_watermark_buf(&mut state.net_total_str, "H↑:", &FormatNetSpeed(state.net_total_tracker.1.max_val), &state.net_total_tracker.1.max_color, is_blink_active(state.net_total_tracker.1.last_max_peak), toggle, false);

        print_line(&mut row, &state.net_total_str, &mut stdout)?;
        print_aligned_2col_grid(&mut row, &state.net_display_pool[..net_idx], max_w as usize, &mut stdout)?;

        // --- Render Storage Telemetry (Dynamic Split) ---
        while state.disk_cap_pool.len() < state.disk_nodes.len() {
            state.disk_cap_pool.push(String::with_capacity(256));
            state.disk_io_pool.push(String::with_capacity(256));
            state.disk_combined_pool.push(String::with_capacity(512));
            state.disk_io_split_pool.push(String::with_capacity(512));
        }

        let mut max_cap_len = 0;
        let mut max_io_len = 0;
        let violet = "\x1b[1;38;2;238;130;238m";

        for (i, node) in state.disk_nodes.iter().enumerate() {
            let cap_buf = &mut state.disk_cap_pool[i];
            let io_buf = &mut state.disk_io_pool[i];
            cap_buf.clear();
            io_buf.clear();

            let _ = write!(cap_buf, "{}{}{}\x1b[0m \x1b[0;37mUsed\x1b[0m / \x1b[1;38;2;0;255;255m{}\x1b[0m \x1b[0;37mTotal\x1b[0m",
                node.mount_padded, node.capacity_color, FormatSize(node.used_bytes), FormatSize(node.total_bytes));

            let hw = state.disk_trackers.get(&node.raw_dev_name).unwrap();
            let r_color = get_exp_disk_speed_color(node.r_speed, hw.current_r);
            let w_color = get_exp_disk_speed_color(node.w_speed, hw.current_w);

            let _ = write!(io_buf, "{}\x1b[1m{}\x1b[0m \x1b[1;37mR↑\x1b[0m  {}\x1b[1m{}\x1b[0m \x1b[1;37mW↓\x1b[0m  ",
                r_color, FormatDiskSpeed(node.r_speed), w_color, FormatDiskSpeed(node.w_speed));
           
            write_watermark_buf(io_buf, "HR:", &FormatDiskSpeed(hw.current_r), &violet, is_blink_active(hw.last_r_peak), toggle, false);
            let _ = write!(io_buf, "  ");
            write_watermark_buf(io_buf, "HW:", &FormatDiskSpeed(hw.current_w), &violet, is_blink_active(hw.last_w_peak), toggle, false);

            let c_len = strip_ansi(cap_buf);
            if c_len > max_cap_len { max_cap_len = c_len; }
            let i_len = strip_ansi(io_buf);
            if i_len > max_io_len { max_io_len = i_len; }
        }

        let agg_percent = if state.disk_agg_total > 0 { state.disk_agg_used as f64 / state.disk_agg_total as f64 } else { 0.0 };
        let force_violet_parent = state.disk_nodes.iter().any(|n| (n.used_bytes as f64 / n.total_bytes as f64) >= 0.95);
        let parent_color = if force_violet_parent { violet.to_string() } else { get_disk_capacity_color(agg_percent, false) };

        let r_parent_color = get_exp_disk_speed_color(state.disk_agg_read, state.disk_global_hw.current_r);
        let w_parent_color = get_exp_disk_speed_color(state.disk_agg_write, state.disk_global_hw.current_w);

        state.disk_cap_parent.clear();
        let _ = write!(&mut state.disk_cap_parent, "\x1b[1mStorage Space Total:\x1b[0m {p_col}\x1b[1m{}\x1b[0m \x1b[1;37mUsed\x1b[0m | \x1b[1;38;2;0;255;255m{}\x1b[0m \x1b[1;37mTotal\x1b[0m | {p_col}\x1b[1m{:.1}%\x1b[0m \x1b[0;37m%Used\x1b[0m",
            FormatSize(state.disk_agg_used), FormatSize(state.disk_agg_total), agg_percent * 100.0, p_col=parent_color);

        state.disk_io_parent.clear();
        let _ = write!(&mut state.disk_io_parent, "\x1b[1mStorage \x1b[1m↓↑\x1b[0m \x1b[1mTotal:\x1b[0m {}\x1b[1m{}\x1b[0m \x1b[1;37mR↑\x1b[0m  {}\x1b[1m{}\x1b[0m \x1b[1;37mW↓\x1b[0m  ",
            r_parent_color, FormatDiskSpeed(state.disk_agg_read), w_parent_color, FormatDiskSpeed(state.disk_agg_write));

        write_watermark_buf(&mut state.disk_io_parent, "HR:", &FormatDiskSpeed(state.disk_global_hw.current_r), &violet, is_blink_active(state.disk_global_hw.last_r_peak), toggle, false);
        let _ = write!(&mut state.disk_io_parent, "  ");
        write_watermark_buf(&mut state.disk_io_parent, "HW:", &FormatDiskSpeed(state.disk_global_hw.current_w), &violet, is_blink_active(state.disk_global_hw.last_w_peak), toggle, false);

        state.disk_combined_parent.clear();
        let _ = write!(&mut state.disk_combined_parent, "\x1b[1mStorage Total:\x1b[0m {p_col}\x1b[1m{}\x1b[0m \x1b[1;37mUsed\x1b[0m | \x1b[1;38;2;0;255;255m{}\x1b[0m \x1b[1;37mTotal\x1b[0m | {p_col}\x1b[1m{:.1}%\x1b[0m \x1b[0;37m%Used\x1b[0m | {}\x1b[1m{}\x1b[0m \x1b[1;37mR↑\x1b[0m  {}\x1b[1m{}\x1b[0m \x1b[1;37mW↓\x1b[0m  ",
            FormatSize(state.disk_agg_used), FormatSize(state.disk_agg_total), agg_percent * 100.0,
            r_parent_color, FormatDiskSpeed(state.disk_agg_read), w_parent_color, FormatDiskSpeed(state.disk_agg_write), p_col=parent_color);

        write_watermark_buf(&mut state.disk_combined_parent, "HR:", &FormatDiskSpeed(state.disk_global_hw.current_r), &violet, is_blink_active(state.disk_global_hw.last_r_peak), toggle, false);
        let _ = write!(&mut state.disk_combined_parent, "  ");
        write_watermark_buf(&mut state.disk_combined_parent, "HW:", &FormatDiskSpeed(state.disk_global_hw.current_w), &violet, is_blink_active(state.disk_global_hw.last_w_peak), toggle, false);

        let disk_node_count = state.disk_nodes.len();
        if max_cap_len + 3 + max_io_len <= max_w as usize {
            if row < max_h {
                stdout.queue(cursor::MoveTo(0, row))?;
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
                write!(stdout, "{}", get_dashed_line(max_w as usize, "\x1b[1;38;2;255;215;0mStorage Heat-Map\x1b[0m"))?;
                row += 1;
            }
            print_line(&mut row, &state.disk_combined_parent, &mut stdout)?;
            for i in 0..disk_node_count {
                let com_buf = &mut state.disk_combined_pool[i];
                com_buf.clear();
                let pad = " ".repeat(max_cap_len.saturating_sub(strip_ansi(&state.disk_cap_pool[i])));
                let _ = write!(com_buf, "{}{} | {}", state.disk_cap_pool[i], pad, state.disk_io_pool[i]);
            }
            print_aligned_2col_grid(&mut row, &state.disk_combined_pool[..disk_node_count], max_w as usize, &mut stdout)?;
        } else {
            if row < max_h {
                stdout.queue(cursor::MoveTo(0, row))?;
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
                write!(stdout, "{}", get_dashed_line(max_w as usize, "\x1b[1;38;2;255;215;0mStorage Space Heat-Map\x1b[0m"))?;
                row += 1;
            }
            print_line(&mut row, &state.disk_cap_parent, &mut stdout)?;
            print_aligned_2col_grid(&mut row, &state.disk_cap_pool[..disk_node_count], max_w as usize, &mut stdout)?;

            if row < max_h {
                stdout.queue(cursor::MoveTo(0, row))?;
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
                write!(stdout, "{}", get_dashed_line(max_w as usize, "\x1b[1;38;2;255;215;0mStorage \x1b[1m↓↑\x1b[0m \x1b[1;38;2;255;215;0mHeat-Map\x1b[0m"))?;
                row += 1;
            }
            print_line(&mut row, &state.disk_io_parent, &mut stdout)?;
           
            for i in 0..disk_node_count {
                let buf = &mut state.disk_io_split_pool[i];
                buf.clear();
                let _ = write!(buf, "{}{}", state.disk_nodes[i].mount_padded, state.disk_io_pool[i]);
            }
            print_aligned_2col_grid(&mut row, &state.disk_io_split_pool[..disk_node_count], max_w as usize, &mut stdout)?;
        }

        if row < max_h {
            stdout.queue(cursor::MoveTo(0, row))?;
            if row == max_h - 1 {
                write!(stdout, "{}", "-".repeat((max_w as usize).saturating_sub(1)))?;
            } else {
                write!(stdout, "{}", "-".repeat(max_w as usize))?;
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            }
            row += 1;
        }

        if row < max_h {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::FromCursorDown))?;
        }

        // Reset dirty state flag and flush terminal stdout buffer
        state.screen_is_dirty = false;
        stdout.flush()?;
    }

    // Terminal raw mode teardown block safely anchored outside loop
    terminal::disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
    Ok(())
}
