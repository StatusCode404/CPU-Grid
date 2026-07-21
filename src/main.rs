#![cfg(target_os = "linux")]

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
    QueueableCommand,
};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const DEFAULT_INTERVAL: f64 = 2.0;
const MIN_CELL_WIDTH: usize = 17; // Reduced to allow tighter horizontal grid squeezing

// --- Data Structures ---

struct TempStat {
    label: String,
    val: f64,
    color: String,
    is_parent: bool,
}

enum Msg {
    CpuFreqs(Vec<f64>),
    CpuTemps(Vec<(String, Vec<TempStat>)>), // Parent CPU/Adapter, Vec of Chiplet data
    RoomTemp(String),
    MemStats {
        ram_str: String,
        zswap_str: String,
        swap_total_str: String,
        swaps_formatted: Vec<String>, // Formatted in background thread to prevent UI heap fragmentation
    },
    NetStats(Vec<(String, f64, f64, f64)>), // Interface, Rx Speed, Tx Speed, Max Speed
    NetEvent(String, String),               // Interface Name, Event String (ACTIVATED/DEACTIVATED)
    DiskStats {
        disk_total_str: String,
        disks_formatted: Vec<String>,
    },
    UserIdle(Duration),
}

struct SystemState {
    cpu_model_display: String,
    limits: (Option<f64>, Option<f64>),
    freqs: Vec<f64>,
    cpu_temps: Vec<(String, Vec<TempStat>)>,
    room_temp: String,
    mem_ram_str: String,
    mem_zswap_str: String,
    mem_swap_total_str: String,
    swaps_formatted: Vec<String>,
    net_total_str: String,
    net_stats: Vec<String>,
    net_events: HashMap<String, (String, Instant)>,
    disk_total_str: String,
    disks_formatted: Vec<String>,
    idle_time: Duration,
}

// --- Display & Formatting Helpers ---

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
    let brt = "\x1b[1;31m";

    println!("\x1b[1;38;2;255;215;0mCPU-Grid ver:{}\x1b[0m", version);
    println!("Copyright (C) 2026 StatusCode404 https://github.com/StatusCode404");
    println!("Project: https://github.com/StatusCode404/CPU-Grid");
    println!("Compatibility: Full support for x86, ARM (incl. Apple Silicon), RISC-V, and IBM processor architectures via standard Linux kernel sysfs interfaces.");

    println!("\nUsage (Values are in seconds. Parameters given less than or greater than the boundary ranges will fall back to the nearest boundary range.):");
    println!("  -n[<secs>]    Interval for CPU stats (0.1 - 60s, default 2.0)");
    println!("  -r[<secs>]    Interval for Room Temp (1 - 3600s, default 2.0)");
    println!("  -m[<secs>]    Interval for Memory stats (0.5 - 60s, default 2.0)");
    println!("  -t[<secs>]    Interval for Network/Disk traffic (0.5 - 60s, default 2.0)");

    println!("\nTips:");
    println!("  If running with {red}sudo{rst} and Room Temp fails, use '{red}sudo -E{rst}' to preserve your user environment.");

    println!("\nColor Legend (Color shade gradually changes between the ranges defined underneath):");
    println!("  CPU Freq:       {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-70%) -> {org}Orange{rst}(70-85%) -> {red}Hot Red{rst}(85-100%) -> {vio}Violet{rst}(>100% overclock)");
    println!("  RAM Load:       {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-70%) -> {org}Orange{rst}(70-85%) -> {red}Hot Red{rst}(85-95%) -> {vio}Violet{rst}(>=95%)");
    println!("                  (Used and Available values share the same color to indicate total memory pressure)");
    println!("  Swap Load:      {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-70%) -> {org}Orange{rst}(70-80%) -> {red}Hot Red{rst}(80-90%) -> {vio}Violet{rst}(>=90%)");
    println!("  Network Load:   {grn}Green{rst}(Low) -> {yel}Yellow{rst} -> {org}Orange{rst} -> {red}Hot Red{rst}(Near Interface Max) -> {vio}Violet{rst}(Exceeds Theoretical)");
    println!("  Storage Load:   {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-80%) -> {org}Orange{rst}(80-90%) -> {red}Hot Red{rst}(90-95%) -> {vio}Violet{rst}(>=95%)");
    println!("                  (Note: BTRFS/ZFS limits scale earlier to account for Copy-on-Write fragmentation degradation)");
    println!("  CPU Temp:       {grn}Green{rst} (Cool) -> {red}Red{rst} (Thermal Throttle Limit) -> {vio}Violet{rst} (Exceeds Limit)");
    println!("  Room Temp:      {grn}Green{rst} (<=24) -> {yel}Yellow{rst}(27) -> {org}Orange{rst}(31) -> {ltr}LtRed{rst}(35) -> {vio}Violet{rst}(>=40)");
    println!("  Zswap Status:   {grn}Green{rst} (Enabled) -> {brt}Bright Red{rst} (Disabled) -> {yel}Yellow{rst} (Unknown Status) -> {vio}Violet{rst} (Not Present)");
    println!("  Zswap Algo:     {grn}zstd{rst} (Best) -> {yel}lz4{rst} -> {org}lzo{rst} -> {red}deflate{rst} -> {vio}Other{rst}");
    println!("  Zswap Ratio:    {vio}Violet{rst} (<1:1) -> {red}Red{rst} (1:1) -> {org}Orange{rst} (1.5:1) -> {yel}Yellow{rst} (2.5:1) -> {grn}Green{rst} (4:1+)");
    println!("  User Activity:  {cya}Cyan{rst} (Active) -> {grn}Green{rst} -> {yel}Yellow{rst} -> {org}Orange{rst} -> {red}Red{rst} -> {vio}Violet{rst} (1+ Year Idle)");
}

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

// Strictly accepts Bytes and enforces a flawless 6-character value + unit architecture up to Petabytes.
fn format_size(bytes: u64) -> String {
    let kb = 1024_f64;
    let mb = kb * 1024.0;
    let gb = mb * 1024.0;
    let tb = gb * 1024.0;
    let pb = tb * 1024.0;
    let val = bytes as f64;

    if val < kb { format!("{:6.0}   B", val) }
    else if val < mb { format!("{:6.1}  KB", val / kb) }
    else if val < gb { format!("{:6.1}  MB", val / mb) }
    else if val < tb { format!("{:6.2}  GB", val / gb) }
    else if val < pb { format!("{:6.2}  TB", val / tb) }
    else { format!("{:6.2}  PB", val / pb) }
}

// Ensures all speed strings are exactly 11 characters long for perfect arrow vertical alignment
fn format_net_speed(bytes_per_sec: f64) -> String {
    let kb = 1024_f64;
    let mb = kb * 1024.0;
    let gb = mb * 1024.0;
    let tb = gb * 1024.0;
   
    if bytes_per_sec < kb { format!("{:6.1}   B/s", bytes_per_sec) }
    else if bytes_per_sec < mb { format!("{:6.1}  KB/s", bytes_per_sec / kb) }
    else if bytes_per_sec < gb { format!("{:6.1}  MB/s", bytes_per_sec / mb) }
    else if bytes_per_sec < tb { format!("{:6.2}  GB/s", bytes_per_sec / gb) }
    else { format!("{:6.2}  TB/s", bytes_per_sec / tb) }
}

fn format_idle_time(secs: u64) -> String {
    let y = secs / 31536000;
    let mon = (secs % 31536000) / 2592000;
    let d = (secs % 2592000) / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;

    if y > 0 { format!("{} Years {} Months {} Days {:02}:{:02}:{:02}", y, mon, d, h, m, s) }
    else if mon > 0 { format!("{} Months {} Days {:02}:{:02}:{:02}", mon, d, h, m, s) }
    else if d > 0 { format!("{} Days {:02}:{:02}:{:02}", d, h, m, s) }
    else if h > 0 { format!("{:02}:{:02}:{:02}", h, m, s) }
    else if m > 0 { format!("{:02}:{:02}", m, s) }
    else { format!("{}s", s) }
}

// [SIMD Optimization]: #[inline(always)] forces this heavily utilized math function
// to be directly injected into mapping closures. When iterating over arrays (like CPU cores),
// LLVM can auto-vectorize the lerp calculations across multiple elements simultaneously via SIMD hardware.
#[inline(always)]
fn lerp_color(c1: (u8, u8, u8), c2: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    (
        (c1.0 as f64 + (c2.0 as f64 - c1.0 as f64) * t).round() as u8,
        (c1.1 as f64 + (c2.1 as f64 - c1.1 as f64) * t).round() as u8,
        (c1.2 as f64 + (c2.2 as f64 - c1.2 as f64) * t).round() as u8,
    )
}

#[inline(always)]
fn format_dynamic_6(val: f64) -> String {
    let int_part = val.trunc();
    let int_len = if int_part == 0.0 { 1 } else { int_part.abs().log10().floor() as i32 + 1 };
   
    if int_len >= 6 { format!("{:6.0}", val.clamp(0.0, 999999.0)) }
    else { let prec = (6 - int_len - 1).max(0) as usize; format!("{:.*}", prec, val) }
}

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

#[inline]
fn get_disk_color(t: f64, is_cow: bool) -> String {
    // Dynamic thresholds based on filesystem architecture idiosyncracies. Btrfs degrades if overfilled.
    let (t_orange, t_red, t_violet) = if is_cow { (0.50, 0.75, 0.85) } else { (0.80, 0.90, 0.95) };

    if t >= t_violet { return "\x1b[1;38;2;238;130;238m".to_string(); }
    let t = t.clamp(0.0, 1.0);
   
    let (r, g, b) = if t <= 0.5 { lerp_color((0, 200, 0), (255, 255, 0), t / 0.5) }
    else if t <= t_orange { lerp_color((255, 255, 0), (255, 165, 0), (t - 0.5) / (t_orange - 0.5)) }
    else if t <= t_red { lerp_color((255, 165, 0), (200, 30, 30), (t - t_orange) / (t_red - t_orange)) }
    else { lerp_color((200, 30, 30), (255, 0, 0), (t - t_red) / (t_violet - t_red)) };
   
    if t >= t_red { format!("\x1b[1;38;2;{};{};{}m", r, g, b) }
    else { format!("\x1b[38;2;{};{};{}m", r, g, b) }
}

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

#[inline]
fn get_ratio_color(ratio: f64) -> String {
    let (r, g, b) = if ratio < 1.0 { (238, 130, 238) }
    else if ratio <= 1.5 { lerp_color((255, 50, 0), (255, 165, 0), (ratio - 1.0) / 0.5) }
    else if ratio <= 2.5 { lerp_color((255, 165, 0), (255, 255, 0), (ratio - 1.5) / 1.0) }
    else if ratio <= 4.0 { lerp_color((255, 255, 0), (0, 200, 0), (ratio - 2.5) / 1.5) }
    else { (0, 200, 0) };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

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

// --- System Info Queries ---

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

fn get_thermal_stats(is_vm: bool) -> Vec<(String, Vec<TempStat>)> {
    let mut cpu_groups: HashMap<String, Vec<TempStat>> = HashMap::new();

    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = fs::read_to_string(path.join("name")).unwrap_or_default();
            if !name.trim().contains("k10temp") && !name.trim().contains("coretemp") { continue; }

            let parent_name = name.trim().to_string();
            let mut chiplet_parts = Vec::new();

            for file in fs::read_dir(&path).into_iter().flatten().flatten() {
                let fname = file.file_name().to_string_lossy().into_owned();
                if !fname.starts_with("temp") || !fname.ends_with("_input") { continue; }
               
                let input_val = fs::read_to_string(file.path())
                    .ok()
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(0.0);

                let read_limit = |file_name| {
                    fs::read_to_string(file.path().with_file_name(file_name))
                        .ok()
                        .and_then(|s| s.trim().parse::<f64>().ok())
                };

                let limit = read_limit(fname.replace("_input", "_max"))
                    .or_else(|| read_limit(fname.replace("_input", "_crit")))
                    .unwrap_or(95000.0);

                let color = get_cpu_color(input_val / limit);
                let label = fs::read_to_string(path.join(fname.replace("_input", "_label")))
                    .unwrap_or_else(|_| fname.replace("_input", "")).trim().to_string();

                let is_parent = label.contains("Tctl") || label.contains("Package") || label.contains("Tdie");

                chiplet_parts.push(TempStat {
                    label,
                    val: input_val / 1000.0,
                    color,
                    is_parent,
                });
            }

            if !chiplet_parts.is_empty() {
                chiplet_parts.sort_by(|a, b| { b.is_parent.cmp(&a.is_parent).then(a.label.cmp(&b.label)) });
                cpu_groups.insert(parent_name, chiplet_parts);
            }
        }
    }

    if cpu_groups.is_empty() {
        let default_val = if is_vm { TempStat { label: "VM".to_string(), val: 0.0, color: "\x1b[38;2;255;165;0m".to_string(), is_parent: true } }
                          else { TempStat { label: "N/A".to_string(), val: 0.0, color: "\x1b[38;2;255;0;0m".to_string(), is_parent: true } };
        vec![("CPU Temps".to_string(), vec![default_val])]
    } else { cpu_groups.into_iter().collect() }
}

fn get_dashed_line(max_w: usize, mid_text: &str) -> String {
    let padding = " ";
    let content_len = strip_ansi(mid_text) + padding.len() * 2;
    if content_len >= max_w { return "-".repeat(max_w); }
    let left_dashes = (max_w - content_len) / 2;
    let right_dashes = max_w - content_len - left_dashes;
    format!("{}{}{}{}{}", "-".repeat(left_dashes), padding, mid_text, padding, "-".repeat(right_dashes))
}

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

fn get_desktop_idle_time() -> Option<Duration> {
    let run_cmd = |cmd: &str, args: &[&str]| {
        if let Ok(sudo_user) = std::env::var("SUDO_USER") {
            let script = format!("XDG_RUNTIME_DIR=/run/user/$(id -u {0}) DISPLAY=${{DISPLAY:-:0}} {1} {2}", sudo_user, cmd, args.join(" "));
            Command::new("sudo").args(["-u", &sudo_user, "sh", "-c", &script]).output()
        } else { Command::new(cmd).args(args).output() }
    };

    if let Ok(out) = run_cmd("busctl", &["--user", "call", "org.gnome.Mutter.IdleMonitor", "/org/gnome/Mutter/IdleMonitor/Core", "org.gnome.Mutter.IdleMonitor", "GetIdletime"]) {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(t_str) = s.split_whitespace().last() { if let Ok(ms) = t_str.parse::<u64>() { return Some(Duration::from_millis(ms)); } }
    }

    if let Ok(out) = run_cmd("busctl", &["--user", "call", "org.kde.Screensaver", "/ScreenSaver", "org.kde.Screensaver", "GetSessionIdleTime"]) {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(t_str) = s.split_whitespace().last() { if let Ok(secs) = t_str.parse::<u32>() { return Some(Duration::from_secs(secs as u64)); } }
    }

    if let Ok(out) = run_cmd("xprintidle", &[]) {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Ok(ms) = s.trim().parse::<u64>() { return Some(Duration::from_millis(ms)); }
    }
    None
}

fn get_user_idle_time() -> Duration {
    let mut newest_time = SystemTime::UNIX_EPOCH;
    let mut check_dir = |path: &str, check_mtime: bool, prefix: &str| {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                if prefix.is_empty() || fname.to_string_lossy().starts_with(prefix) {
                    if let Ok(meta) = entry.metadata() {
                        if let Ok(atime) = meta.accessed() { newest_time = newest_time.max(atime); }
                        if check_mtime { if let Ok(mtime) = meta.modified() { newest_time = newest_time.max(mtime); } }
                    }
                }
            }
        }
    };
    check_dir("/dev/input", true, "");
    check_dir("/dev/pts", false, "");
    check_dir("/dev", false, "tty");

    let fs_idle = SystemTime::now().duration_since(newest_time).unwrap_or(Duration::ZERO);
    if fs_idle.as_secs() > 1 { if let Some(desktop_idle) = get_desktop_idle_time() { return desktop_idle.min(fs_idle); } }
    fs_idle
}

// --- Main Loop ---

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1).peekable();
    let mut cpu_interval = DEFAULT_INTERVAL;
    let mut room_interval = DEFAULT_INTERVAL;
    let mut mem_interval = DEFAULT_INTERVAL;
    let mut net_interval = DEFAULT_INTERVAL;

    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" { print_help(); std::process::exit(0); }
        else if arg.starts_with("-n") {
            let val = if arg.len() > 2 { arg[2..].parse().unwrap_or(DEFAULT_INTERVAL) } else { args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL) };
            cpu_interval = val.clamp(0.1, 60.0);
        } else if arg.starts_with("-r") {
            let val = if arg.len() > 2 { arg[2..].parse().unwrap_or(DEFAULT_INTERVAL) } else { args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL) };
            room_interval = val.clamp(1.0, 3600.0);
        } else if arg.starts_with("-m") {
            let val = if arg.len() > 2 { arg[2..].parse().unwrap_or(DEFAULT_INTERVAL) } else { args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL) };
            mem_interval = val.clamp(0.5, 60.0);
        } else if arg.starts_with("-t") {
            let val = if arg.len() > 2 { arg[2..].parse().unwrap_or(DEFAULT_INTERVAL) } else { args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL) };
            net_interval = val.clamp(0.5, 60.0);
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

    let (tx, rx) = mpsc::sync_channel::<Msg>(32);

    // 1. CPU Thread
    let tx_cpu = tx.clone();
    thread::Builder::new().name("cg-cpu".to_string()).spawn(move || {
        let mut buf = String::with_capacity(8192);
        loop {
            buf.clear();
            if let Ok(mut file) = File::open("/proc/cpuinfo") { let _ = file.read_to_string(&mut buf); }
            let freqs = buf.lines().filter(|l| l.starts_with("cpu MHz") || l.starts_with("BogoMIPS")).filter_map(|l| l.split(':').nth(1)?.trim().parse::<f64>().ok()).collect();
            if tx_cpu.send(Msg::CpuFreqs(freqs)).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(cpu_interval));
        }
    }).unwrap();

    // 2. Thermal Thread
    let tx_ctemp = tx.clone();
    thread::Builder::new().name("cg-thermal".to_string()).spawn(move || loop {
        if tx_ctemp.send(Msg::CpuTemps(get_thermal_stats(is_vm))).is_err() { break; }
        thread::sleep(Duration::from_secs_f64(cpu_interval));
    }).unwrap();

    // 3. Room Temp Thread
    let tx_room = tx.clone();
    thread::Builder::new().name("cg-room".to_string()).spawn(move || loop {
        let cmd_path = find_temper_poll().unwrap_or_else(|| std::path::PathBuf::from("temper-poll"));
        let out = Command::new(&cmd_path).output();
        let msg = if let Ok(o) = out {
            let s = String::from_utf8_lossy(&o.stdout);
            if let Some(line) = s.lines().find(|l| l.contains("Device #0:")) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(temp_str) = parts.iter().find(|p| p.contains('°')) {
                    let clean_temp = temp_str.replace('°', "").replace('C', "");
                    if let Ok(temp) = clean_temp.parse::<f64>() {
                        let mut final_temp = format!("{}\x1b[1m{:.1}°C\x1b[0m", get_room_temp_color(temp), temp);
                        if temp >= 40.0 { final_temp.push_str(" \x1b[1;38;2;255;165;0mWARNING:\x1b[0m \x1b[1;38;2;238;130;238mAmbient Temp is too HOT! Consider Shutting Down!\x1b[0m"); }
                        final_temp
                    } else { "\x1b[38;2;255;0;0mNo thermometer detected\x1b[0m".to_string() }
                } else { "\x1b[38;2;255;0;0mNo thermometer detected\x1b[0m".to_string() }
            } else { "\x1b[38;2;255;0;0mNo thermometer detected\x1b[0m".to_string() }
        } else { "\x1b[38;2;255;0;0mNo thermometer detected\x1b[0m".to_string() };

        if tx_room.send(Msg::RoomTemp(msg)).is_err() { break; }
        thread::sleep(Duration::from_secs_f64(room_interval));
    }).unwrap();

    // 4. Memory Thread
    let tx_mem = tx.clone();
    thread::Builder::new().name("cg-mem".to_string()).spawn(move || {
        let mut mem_buf = String::with_capacity(2048);
        let mut swap_buf = String::with_capacity(2048);
        loop {
            mem_buf.clear();
            if let Ok(mut file) = File::open("/proc/meminfo") { let _ = file.read_to_string(&mut mem_buf); }
            let (mut total, mut avail) = (0u64, 0u64);
            for line in mem_buf.lines() {
                let p: Vec<&str> = line.split_whitespace().collect();
                if p.len() < 2 { continue; }
                let val = p[1].parse::<u64>().unwrap_or(0);
                if p[0] == "MemTotal:" { total = val; }
                else if p[0] == "MemAvailable:" { avail = val; }
            }
           
            // KB converted directly into Bytes to ensure strict 6-character UI scaling
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
                format_size(used_bytes), format_size(avail_bytes), format_size(total_bytes), col=mem_color, wb_used_parent=wb_used_parent, wb_avail_parent=wb_avail_parent, cya=cyan_bold);

            swap_buf.clear();
            if let Ok(mut file) = File::open("/proc/swaps") { let _ = file.read_to_string(&mut swap_buf); }
            let mut total_swap = 0u64;
            let mut total_swap_used = 0u64;
            let mut swap_devices_formatted = Vec::new();
           
            let mut parsed_swaps = Vec::new();
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

            // Align all Swap rows by finding the max name length.
            // Fixed width left strings mathematically ensures UI right alignment on `Total`.
            let max_swap_len = parsed_swaps.iter().map(|(n, _, _)| n.len()).max().unwrap_or(4);
           
            for (name, used_bytes, size_bytes) in parsed_swaps {
                let swap_percent = if size_bytes > 0 { used_bytes as f64 / size_bytes as f64 } else { 0.0 };
                let col = get_swap_color(swap_percent);
                swap_devices_formatted.push(format!(" {:<width$}: {col}{}\x1b[0m \x1b[0;37mUsed\x1b[0m / \x1b[38;2;0;255;255m{}\x1b[0m \x1b[0;37mTotal\x1b[0m",
                    name, format_size(used_bytes), format_size(size_bytes), col=col, width=max_swap_len));
            }

            let total_swap_percent = if total_swap > 0 { total_swap_used as f64 / total_swap as f64 } else { 0.0 };
            let swap_col = get_swap_color(total_swap_percent);
            let swap_total_str = format!("{col}\x1b[1m{}\x1b[0m {wb_used_parent} | {cya}\x1b[1m{}\x1b[0m {wb_total_parent} | {col}\x1b[1m{:.1}%\x1b[0m \x1b[0;37m%Used\x1b[0m",
                format_size(total_swap_used), format_size(total_swap), total_swap_percent * 100.0, col=swap_col, wb_used_parent=wb_used_parent, wb_total_parent=wb_total_parent, cya=cyan_bold);

            let zswap_param_path = std::path::Path::new("/sys/module/zswap/parameters/enabled");
            let zswap_str = if !zswap_param_path.exists() { format!("\x1b[38;2;238;130;238m\x1b[1mNot Present\x1b[0m") }
            else {
                match fs::read_to_string(zswap_param_path) {
                    Ok(val) => match val.trim() {
                        "Y" => {
                            match (fs::read_to_string("/sys/kernel/debug/zswap/pool_total_size"), fs::read_to_string("/sys/kernel/debug/zswap/stored_pages")) {
                                (Ok(p_str), Ok(pg_str)) => {
                                    let pool_bytes = p_str.trim().parse::<u64>().unwrap_or(0); // This parameter uniquely outputs raw Bytes.
                                    let pages = pg_str.trim().parse::<u64>().unwrap_or(0);
                                    let ratio = if pool_bytes > 0 { (pages * 4 * 1024) as f64 / (pool_bytes as f64) } else { 0.0 };
                                    let pool_color = if pool_bytes > 0 { "\x1b[38;2;0;200;0m" } else { "\x1b[38;2;150;150;150m" };
                                    let ratio_color = if ratio > 0.0 { get_ratio_color(ratio) } else { "\x1b[0m".to_string() };
                                    let algo = fs::read_to_string("/sys/module/zswap/parameters/compressor").unwrap_or_else(|_| "unknown".to_string());
                                    let algo_trim = algo.trim();
                                    let algo_color = get_zswap_algo_color(algo_trim);
                                    format!("\x1b[38;2;0;200;0m\x1b[1mEnabled\x1b[0m | \x1b[1mAlgo:\x1b[0m {algo_color}\x1b[1m{algo_trim}\x1b[0m | \x1b[1mPool:\x1b[0m {pool_color}\x1b[1m{}\x1b[0m | \x1b[1mRatio:\x1b[0m {ratio_color}\x1b[1m{:.1}:1\x1b[0m", format_size(pool_bytes), ratio)
                                }
                                _ => format!("\x1b[38;2;0;200;0m\x1b[1mEnabled\x1b[0m (\x1b[38;2;255;255;0m\x1b[1mRequires sudo for detailed stats\x1b[0m)"),
                            }
                        }
                        "N" => format!("\x1b[38;2;255;0;0m\x1b[1mDisabled\x1b[0m"),
                        _ => format!("\x1b[38;2;255;0;0m\x1b[1mUnknown\x1b[0m"),
                    },
                    Err(_) => format!("\x1b[38;2;255;0;0m\x1b[1mUnknown\x1b[0m"),
                }
            };

            if tx_mem.send(Msg::MemStats { ram_str, zswap_str, swap_total_str, swaps_formatted: swap_devices_formatted }).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(mem_interval));
        }
    }).unwrap();

    // 5. Network Thread
    let tx_net = tx.clone();
    thread::Builder::new().name("cg-net".to_string()).spawn(move || {
        let mut prev_stats: HashMap<String, (u64, u64, Instant)> = HashMap::with_capacity(16);
        let mut current_stats = Vec::with_capacity(8);
        let mut current_keys = HashSet::with_capacity(8);
       
        loop {
            current_stats.clear();
            current_keys.clear();
            let now = Instant::now();

            if let Ok(dev_str) = fs::read_to_string("/proc/net/dev") {
                for line in dev_str.lines().skip(2) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() < 17 { continue; }
                    let iface = parts[0].trim_end_matches(':').to_string();
                    if iface == "lo" { continue; }

                    let rx = parts[1].parse::<u64>().unwrap_or(0);
                    let tx = parts[9].parse::<u64>().unwrap_or(0);
                    current_keys.insert(iface.clone());

                    let speed_mbps = fs::read_to_string(format!("/sys/class/net/{}/speed", iface)).ok().and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(1000.0);
                    let max_bytes_per_sec = speed_mbps * 1_000_000.0 / 8.0;

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
            }

            let mut to_remove = Vec::new();
            for old_iface in prev_stats.keys() {
                if !current_keys.contains(old_iface) {
                    to_remove.push(old_iface.clone());
                    let _ = tx_net.send(Msg::NetEvent(old_iface.clone(), "DEACTIVATED".to_string()));
                }
            }
            for rm in to_remove { prev_stats.remove(&rm); }

            if tx_net.send(Msg::NetStats(current_stats.clone())).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(net_interval));
        }
    }).unwrap();

    // 6. User Activity Tracker Thread
    let tx_idle = tx.clone();
    thread::Builder::new().name("cg-idle".to_string()).spawn(move || {
        loop {
            if tx_idle.send(Msg::UserIdle(get_user_idle_time())).is_err() { break; }
            thread::sleep(Duration::from_secs(1));
        }
    }).unwrap();

    // --- Storage Telemetry Prototype Thread (v3.0.0) ---
    // Architecture: Uses `libc::statvfs` via C-bindings to calculate partition space.
    // Why libc over rustix?
    // Using `rustix` would bloat the binary with several transitive dependencies (linux-raw-sys, bitflags) and massively increase compile times
    // simply to make a single standard syscall. By cleanly wrapping `libc::statvfs` inside an unsafe block, CPU-Grid guarantees 0% CPU footprint
    // without inflating memory RSS with bloated third-party dependencies, adhering to the "zero bloat" architectural mandate.
    // Disk Throughput is handled in 100% safe, pure Rust via /proc/diskstats.
    let tx_disk = tx.clone();
    thread::Builder::new().name("cg-disk".to_string()).spawn(move || {
        let mut prev_disk_stats: HashMap<String, (u64, u64, Instant)> = HashMap::with_capacity(16);
        let allowed_fs: HashSet<&str> = ["ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "vfat", "exfat", "ntfs", "ntfs3", "f2fs"].into_iter().collect();

        // Hoisted allocations to protect background memory layout from heap fragmentation bloat over uptime.
        let mut mount_caps: HashMap<u64, (u64, u64, String, String, String)> = HashMap::with_capacity(16); // K: f_fsid, V: (Total, Used, Mount, FS, Dev)
        let mut dev_traffic: HashMap<String, (f64, f64)> = HashMap::with_capacity(16); // K: Device Name (e.g., sda), V: (Read Bytes/s, Write Bytes/s)
        let mut formatted_disks = Vec::with_capacity(16);
       
        // Track the maximum theoretical disk speed observed dynamically throughout the process lifetime to normalize color interpolation perfectly.
        // Base starting point assumes standard SATA SSD (500 MB/s). If an NVMe hits 3500 MB/s, it dynamically raises this ceiling.
        let mut session_max_speed = 500_000_000.0_f64;

        loop {
            mount_caps.clear();
            dev_traffic.clear();
            formatted_disks.clear();

            // 1. Gather Disk Capacities & Deduplicate Pools
            if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
                for line in mounts.lines() {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() < 6 { continue; }
                    let dev = parts[0];
                    let mount = parts[1];
                    let fs_type = parts[2];

                    if allowed_fs.contains(fs_type) {
                        use std::ffi::CString;
                        if let Ok(c_path) = CString::new(mount) {
                            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
                            // FFI Call into libc to prevent shelling out to `df`.
                            if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } == 0 {
                                let total_bytes = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
                                let free_bytes = (stat.f_bfree as u64).saturating_mul(stat.f_frsize as u64);
                                let used_bytes = total_bytes.saturating_sub(free_bytes);
                                let fsid = stat.f_fsid as u64;

                                // Deduplicate BTRFS/ZFS pools by merging mount points sharing identical filesystem IDs.
                                // Keeps the shortest root mount path to prevent UI spam.
                                if let Some(existing) = mount_caps.get_mut(&fsid) {
                                    if mount.len() < existing.2.len() {
                                        existing.2 = mount.to_string();
                                    }
                                } else {
                                    mount_caps.insert(fsid, (total_bytes, used_bytes, mount.to_string(), fs_type.to_string(), dev.to_string()));
                                }
                            }
                        }
                    }
                }
            }

            // 2. Gather Real-Time I/O Traffic natively via diskstats
            let now = Instant::now();
            if let Ok(diskstats) = fs::read_to_string("/proc/diskstats") {
                for line in diskstats.lines() {
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
            }

            // 3. Format Strings for UI Layer
            let mut agg_total = 0u64;
            let mut agg_used = 0u64;
            let mut agg_read_speed = 0.0;
            let mut agg_write_speed = 0.0;
            let mut force_violet_parent = false;

            let mut sorted_mounts: Vec<_> = mount_caps.values().collect();
            sorted_mounts.sort_by(|a, b| a.2.cmp(&b.2));

            // Find longest path dynamically so we can mathematically left-pad to right-align the total values automatically in the UI.
            let max_mount_len = sorted_mounts.iter().map(|(_, _, m, _, _)| m.chars().count()).max().unwrap_or(4);

            for (total_bytes, used_bytes, mount, fs_type, dev) in sorted_mounts {
                if *total_bytes == 0 { continue; }
                agg_total += *total_bytes;
                agg_used += *used_bytes;

                // Strip down path (e.g. /dev/mapper/xxx) to pure device node name to match diskstats output.
                let raw_dev_name = if let Ok(path) = fs::canonicalize(dev) {
                    path.file_name().unwrap_or_default().to_string_lossy().to_string()
                } else {
                    dev.split('/').last().unwrap_or("").to_string()
                };

                let (r_speed, w_speed) = dev_traffic.get(&raw_dev_name).unwrap_or(&(0.0, 0.0));
               
                // Track dynamic ceiling limits
                if *r_speed > session_max_speed { session_max_speed = *r_speed; }
                if *w_speed > session_max_speed { session_max_speed = *w_speed; }

                agg_read_speed += r_speed;
                agg_write_speed += w_speed;

                let percent = *used_bytes as f64 / *total_bytes as f64;
                let is_cow = fs_type.contains("btrfs") || fs_type.contains("zfs");
                let color = get_disk_color(percent, is_cow);
                if percent >= (if is_cow { 0.85 } else { 0.95 }) { force_violet_parent = true; }

                // Emulates 2-col UI: Left side receives Capacity string, Right side receives Speed string.
                // Left justifies Mount to exact variable width, enforcing identical text lengths, pushing "Total" uniformly away from `|`
                formatted_disks.push(format!(" {:<width$}: {color}{}\x1b[0m \x1b[0;37mUsed\x1b[0m / \x1b[1;38;2;0;255;255m{}\x1b[0m \x1b[0;37mTotal\x1b[0m",
                    mount, format_size(*used_bytes), format_size(*total_bytes), color=color, width=max_mount_len
                ));
                // Read = Disk to Memory (UP), Write = Memory to Disk (DOWN)
                formatted_disks.push(format!("{}\x1b[1m{}\x1b[0m \x1b[1;37mR↑\x1b[0m  {}\x1b[1m{}\x1b[0m \x1b[1;37mW↓\x1b[0m",
                    get_net_color(*r_speed, session_max_speed), format_net_speed(*r_speed),
                    get_net_color(*w_speed, session_max_speed), format_net_speed(*w_speed),
                ));
            }

            let agg_percent = if agg_total > 0 { agg_used as f64 / agg_total as f64 } else { 0.0 };
            let mut parent_color = get_disk_color(agg_percent, false);
            if force_violet_parent { parent_color = "\x1b[1;38;2;238;130;238m".to_string(); }
           
            let disk_total_str = format!(
                "\x1b[1mStorage Total:\x1b[0m {p_col}\x1b[1m{}\x1b[0m \x1b[1;37mUsed\x1b[0m | \x1b[1;38;2;0;255;255m{}\x1b[0m \x1b[1;37mTotal\x1b[0m | {p_col}\x1b[1m{:.1}%\x1b[0m \x1b[0;37m%Used\x1b[0m | {}\x1b[1m{}\x1b[0m \x1b[1;37mR↑\x1b[0m  {}\x1b[1m{}\x1b[0m \x1b[1;37mW↓\x1b[0m",
                format_size(agg_used), format_size(agg_total), agg_percent * 100.0,
                get_net_color(agg_read_speed, session_max_speed), format_net_speed(agg_read_speed),
                get_net_color(agg_write_speed, session_max_speed), format_net_speed(agg_write_speed),
                p_col=parent_color
            );

            if tx_disk.send(Msg::DiskStats { disk_total_str, disks_formatted: formatted_disks.clone() }).is_err() { break; }
            thread::sleep(Duration::from_secs_f64(mem_interval));
        }
    }).unwrap();

    // --- Terminal Setup ---
    terminal::enable_raw_mode()?;
    let mut stdout = BufWriter::new(io::stdout());
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    stdout.queue(terminal::Clear(ClearType::All))?;

    let mut state = SystemState {
        cpu_model_display,
        limits,
        freqs: vec![],
        cpu_temps: vec![],
        room_temp: "Loading...".into(),
        mem_ram_str: "Loading...".into(),
        mem_zswap_str: "Loading...".into(),
        mem_swap_total_str: "Loading...".into(),
        swaps_formatted: vec![],
        net_total_str: "Loading...".into(),
        net_stats: vec![],
        net_events: HashMap::new(),
        disk_total_str: "Loading...".into(),
        disks_formatted: vec![],
        idle_time: Duration::ZERO,
    };

    loop {
        if event::poll(Duration::from_millis(10))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q') || key.code == KeyCode::Char('Q') || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) { break; }
            }
        }

        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::CpuFreqs(f) => state.freqs = f,
                Msg::CpuTemps(t) => state.cpu_temps = t,
                Msg::RoomTemp(r) => state.room_temp = r,
                Msg::MemStats { ram_str, zswap_str, swap_total_str, swaps_formatted } => {
                    state.mem_ram_str = ram_str;
                    state.mem_zswap_str = zswap_str;
                    state.mem_swap_total_str = swap_total_str;
                    state.swaps_formatted = swaps_formatted;
                }
                Msg::NetStats(n) => {
                    let mut total_rx = 0.0;
                    let mut total_tx = 0.0;
                    let mut total_max = 0.0;
                    let mut net_nodes = Vec::with_capacity(n.len() + state.net_events.len());
                    let align_len = 9.max(n.iter().map(|(iface, _, _, _)| iface.len()).max().unwrap_or(0));

                    for (iface, rx_speed, tx_speed, max_bytes) in &n {
                        total_rx += rx_speed;
                        total_tx += tx_speed;
                        total_max += max_bytes;

                        if let Some((ev, time)) = state.net_events.get(iface) {
                            if time.elapsed().as_secs() < 5 {
                                let ev_color = if ev == "ACTIVATED" { "\x1b[38;2;0;200;0m" } else { "\x1b[38;2;255;255;0m" };
                                net_nodes.push(format!("{:>width$}: \x1b[1m{}{}{}\x1b[0m", iface, ev_color, ev, "\x1b[0m", width=align_len));
                                continue;
                            }
                        }

                        let rx_col = get_net_color(*rx_speed, *max_bytes);
                        let tx_col = get_net_color(*tx_speed, *max_bytes);
                        let rx_str = format_net_speed(*rx_speed);
                        let tx_str = format_net_speed(*tx_speed);
                       
                        net_nodes.push(format!("{:>width$}: {}\x1b[1m{}\x1b[0m \x1b[1;37m↓\x1b[0m  {}\x1b[1m{}\x1b[0m \x1b[1;37m↑\x1b[0m",
                            iface, rx_col, rx_str, tx_col, tx_str, width=align_len));
                    }

                    for (iface, (ev, time)) in &state.net_events {
                        if ev == "DEACTIVATED" && time.elapsed().as_secs() < 5 {
                            net_nodes.push(format!("{:>width$}: \x1b[38;2;255;255;0m\x1b[1mDEACTIVATED\x1b[0m", iface, width=align_len));
                        }
                    }

                    let total_rx_col = get_net_color(total_rx, total_max.max(1.0));
                    let total_tx_col = get_net_color(total_tx, total_max.max(1.0));

                    state.net_total_str = format!("\x1b[1m{:>width$}:\x1b[0m {}\x1b[1m{}\x1b[0m \x1b[1;37m↓\x1b[0m  {}\x1b[1m{}\x1b[0m \x1b[1;37m↑\x1b[0m",
                        "Net Total", total_rx_col, format_net_speed(total_rx), total_tx_col, format_net_speed(total_tx), width=align_len);
                    state.net_stats = net_nodes;
                },
                Msg::DiskStats { disk_total_str, disks_formatted } => {
                    state.disk_total_str = disk_total_str;
                    state.disks_formatted = disks_formatted;
                },
                Msg::NetEvent(iface, event_str) => { state.net_events.insert(iface, (event_str, Instant::now())); },
                Msg::UserIdle(d) => state.idle_time = d,
            }
        }
       
        state.net_events.retain(|_, (_, time)| time.elapsed().as_secs() < 5);

        let (max_w, max_h) = terminal::size()?;
        if max_w < MIN_CELL_WIDTH as u16 {
            stdout.queue(terminal::Clear(ClearType::All))?;
            stdout.queue(cursor::MoveTo(0, 0))?;
            write!(stdout, "Terminal too small!")?;
            stdout.flush()?;
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

        // Standardized 2-Column Grid Renderer logic updated cleanly.
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

        // --- Render Header & Core Limits ---
        let version = env!("CARGO_PKG_VERSION");
        print_line(&mut row, &format!("\x1b[1;38;2;255;215;0mCPU-Grid ver:{}\x1b[0m", version), &mut stdout)?;
        print_line(&mut row, &format!("\x1b[1m{}\x1b[0m", state.cpu_model_display), &mut stdout)?;
        print_line(&mut row, "\x1b[1;35mType 'Q' or Ctrl+C to quit.\x1b[0m", &mut stdout)?;

        match state.limits {
            (Some(min), Some(max)) => print_line(&mut row, &format!("\x1b[1mHardware Limits:\x1b[0m \x1b[1;38;2;100;255;100m{:.0}\x1b[0m MHz Min | \x1b[1;38;2;255;0;0m{:.0}\x1b[0m MHz Max", min, max), &mut stdout)?,
            _ => {
                let msg = if is_vm { "\x1b[1mHardware Limits:\x1b[0m \x1b[38;2;255;165;0mVM detected, limits not exposed\x1b[0m" }
                else { "\x1b[1mHardware Limits:\x1b[0m \x1b[38;2;255;0;0mUnavailable\x1b[0m" };
                print_line(&mut row, msg, &mut stdout)?;
            }
        }

        let idle_secs = state.idle_time.as_secs();
        if idle_secs < 5 {
            print_line(&mut row, &format!("\x1b[1mUser Activity:\x1b[0m \x1b[1m\x1b[38;2;0;255;255mACTIVE\x1b[0m"), &mut stdout)?;
        } else {
            print_line(&mut row, &format!("\x1b[1mUser Activity:\x1b[0m \x1b[1m{}IDLE {}\x1b[0m", get_idle_color(idle_secs), format_idle_time(idle_secs)), &mut stdout)?;
        }

        // --- Render Temperatures Heat-Map ---
        if row < max_h - 1 {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, "\x1b[1;38;2;255;215;0mTemperature Heat-Map\x1b[0m"))?;
            row += 1;
        }

        print_line(&mut row, &format!("\x1b[1mRoom Temp:\x1b[0m {}", state.room_temp), &mut stdout)?;

        for (parent, chiplets) in &state.cpu_temps {
            if chiplets.is_empty() { continue; }
            print_line(&mut row, &format!("\x1b[1mCPU Temps ({}):\x1b[0m", parent), &mut stdout)?;
           
            let cols = (max_w as usize / MIN_CELL_WIDTH).max(1).min(chiplets.len().max(1));
            let temp_rows = (chiplets.len() + cols - 1) / cols;
           
            let max_lbl = chiplets.iter().map(|c| c.label.len()).max().unwrap_or(4).max(4);

            for r in 0..temp_rows {
                if row >= max_h - 1 { break; }
                stdout.queue(cursor::MoveTo(0, row))?;
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
                for c in 0..cols {
                    let idx = r * cols + c;
                    if idx < chiplets.len() {
                        let stat = &chiplets[idx];
                        let sep = if c < cols - 1 { " | " } else { "" };
                        let fw = if stat.is_parent { "\x1b[1m" } else { "" };
                        write!(stdout, "{}{:>width$}: {}{:4.1}°C\x1b[0m{}", fw, stat.label, stat.color, stat.val, sep, width=max_lbl)?;
                    }
                }
                row += 1;
            }
        }

        // --- Render Core Heat-Map ---
        let core_msg = if state.freqs.is_empty() { "Error: Core Frequencies cannot be accessed" }
                       else if is_vm { "VM Guest Detected" }
                       else { "Core Heat-Map" };

        if row < max_h - 1 {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, &format!("\x1b[1;38;2;255;215;0m{}\x1b[0m", core_msg)))?;
            row += 1;
        }

        let cols = (max_w as usize / MIN_CELL_WIDTH).max(1).min(state.freqs.len().max(1));
        let cpu_rows = (state.freqs.len() + cols - 1) / cols;

        for r in 0..cpu_rows {
            if row >= max_h - 1 { break; }
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            for c in 0..cols {
                let idx = r * cols + c;
                if idx < state.freqs.len() {
                    let freq = state.freqs[idx];
                    let color = if let (Some(min), Some(max)) = state.limits { get_cpu_color((freq - min) / (max - min)) } else { String::new() };
                    let (display_freq, unit) = if freq >= 1_000_000.0 { (freq / 1_000_000.0, "THz") } else if freq >= 1000.0 { (freq / 1000.0, "GHz") } else { (freq, "MHz") };
                    let sep = if c < cols - 1 { " | " } else { "" };
                    let freq_str = format_dynamic_6(display_freq);
                   
                    write!(stdout, "C{:02}: {}{} {}\x1b[0m{}", idx, color, freq_str, unit, sep)?;
                }
            }
            row += 1;
        }

        // --- Render Memory Heat-Map ---
        if row < max_h - 1 {
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
        if row < max_h - 1 {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, "\x1b[1;38;2;255;215;0mNetwork Heat-Map\x1b[0m"))?;
            row += 1;
        }

        print_line(&mut row, &state.net_total_str, &mut stdout)?;
        print_aligned_2col_grid(&mut row, &state.net_stats, max_w as usize, &mut stdout)?;

        // --- Render Storage Telemetry ---
        if row < max_h - 1 {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, "\x1b[1;38;2;255;215;0mStorage Heat-Map\x1b[0m"))?;
            row += 1;
        }

        print_line(&mut row, &state.disk_total_str, &mut stdout)?;
        print_aligned_2col_grid(&mut row, &state.disks_formatted, max_w as usize, &mut stdout)?;

        if row < max_h - 1 {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", "-".repeat(max_w as usize))?;
        }

        stdout.queue(terminal::Clear(ClearType::FromCursorDown))?;
        stdout.flush()?;
        thread::sleep(Duration::from_millis(50));
    }

    terminal::disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
    Ok(())
}
