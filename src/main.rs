use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
    QueueableCommand,
};
use std::env;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const DEFAULT_INTERVAL: f64 = 2.0;
const MIN_CELL_WIDTH: usize = 23;

enum Msg {
    CpuFreqs(Vec<f64>),
    CpuTemps(String),
    RoomTemp(String),
    MemStats(String),
}

struct SystemState {
    cpu_model: String,
    limits: (Option<f64>, Option<f64>),
    freqs: Vec<f64>,
    cpu_temps: String,
    room_temp: String,
    mem_stats: String,
}

// --- Display & Formatting Helpers ---

fn print_help() {
    let version = env!("CARGO_PKG_VERSION");
    
    // ANSI Color Helpers for the help text
    let rst = "\x1b[0m";
    let grn = "\x1b[38;2;0;200;0m";
    let yel = "\x1b[38;2;255;255;0m";
    let org = "\x1b[38;2;255;165;0m";
    let red = "\x1b[38;2;255;0;0m";
    let vio = "\x1b[38;2;238;130;238m";
    let ltr = "\x1b[38;2;255;100;100m";
    let dkr = "\x1b[38;2;139;0;0m";
    let brt = "\x1b[1;31m";

    println!("\x1b[1mCPU-Grid ver:{}\x1b[0m", version);
    println!("Copyright (C) 2026 StatusCode404 https://github.com/StatusCode404");
    println!("Project: https://github.com/StatusCode404/CPU-Grid");
    
    println!("\nUsage (Values are in seconds. Parameters given less than or greater than the boundary ranges will fall back to the nearest boundary range.):");
    println!("  -n[<secs>]    Interval for CPU stats (0.1 - 60s, default 2.0)");
    println!("  -r[<secs>]    Interval for Room Temp (1 - 3600s, default 2.0)");
    println!("  -m[<secs>]    Interval for Memory stats (1 - 60s, default 2.0)");

    println!("\nTips:");
    println!("  If running with {red}sudo{rst} and Room Temp fails, use '{red}sudo -E{rst}' to preserve your user environment.");

    println!("\nColor Legend (Color shade gradually changes between the ranges defined underneath):");
    println!("  CPU Freq:     {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-70%) -> {org}Orange{rst}(70-85%) -> {red}Red{rst}(85-100%) -> {vio}Violet{rst}(>100% overclock)");
    println!("  RAM Load:     {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-70%) -> {org}Orange{rst}(70-85%) -> {red}Red{rst}(85-100%)");
    println!("                (Used and Available values share the same color to indicate total memory pressure)");
    println!("  Swap Load:    {grn}Green{rst}(0-50%) -> {yel}Yellow{rst}(50-70%) -> {org}Orange{rst}(70-85%) -> {red}Red{rst}(85-100%)");
    println!("  CPU Temp:     {grn}Green{rst} (Cool) -> {red}Red{rst} (Thermal Throttle/Critical Limit)");
    println!("                (Note: {red}Red{rst} limit is dynamic, set by your specific CPU hardware)");
    println!("  Room Temp:    {grn}Green{rst} (<=24) -> {yel}Yellow{rst}(25) -> {org}Orange{rst}(30) -> {ltr}LtRed{rst}(35) -> {dkr}DkRed{rst}(40)");
    println!("  Zswap Status: {grn}Green{rst} (Enabled) -> {brt}Bright Red{rst} (Disabled) -> {yel}Yellow{rst} (Unknown Status) -> {dkr}Dark Red{rst} (Not Present)");
    println!("  Zswap Ratio:  {red}Red{rst} (1:1) -> {org}Orange{rst} (1.5:1) -> {yel}Yellow{rst} (2.5:1) -> {grn}Green{rst} (4:1+)");
}

fn format_size(kb: u64) -> String {
    if kb < 1024 {
        format!("{} KB", kb)
    } else if kb < 1024 * 1024 {
        format!("{:.1} MB", kb as f64 / 1024.0)
    } else {
        format!("{:.2} GB", kb as f64 / (1024.0 * 1024.0))
    }
}

fn lerp_color(c1: (u8, u8, u8), c2: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    (
        (c1.0 as f64 + (c2.0 as f64 - c1.0 as f64) * t).round() as u8,
        (c1.1 as f64 + (c2.1 as f64 - c1.1 as f64) * t).round() as u8,
        (c1.2 as f64 + (c2.2 as f64 - c1.2 as f64) * t).round() as u8,
    )
}

fn get_cpu_color(t: f64) -> String {
    if t > 1.0 {
        return "\x1b[38;2;238;130;238m".to_string();
    }
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t <= 0.5 {
        lerp_color((0, 200, 0), (255, 255, 0), t / 0.5)
    } else if t <= 0.7 {
        lerp_color((255, 255, 0), (255, 165, 0), (t - 0.5) / 0.2)
    } else if t <= 0.85 {
        lerp_color((255, 165, 0), (255, 50, 0), (t - 0.7) / 0.15)
    } else {
        lerp_color((255, 50, 0), (139, 0, 0), (t - 0.85) / 0.15)
    };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

fn get_mem_color(t: f64) -> String {
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t <= 0.5 {
        lerp_color((0, 200, 0), (255, 255, 0), t / 0.5)
    } else if t <= 0.7 {
        lerp_color((255, 255, 0), (255, 165, 0), (t - 0.5) / 0.2)
    } else if t <= 0.85 {
        lerp_color((255, 165, 0), (255, 50, 0), (t - 0.7) / 0.15)
    } else {
        lerp_color((255, 50, 0), (139, 0, 0), (t - 0.85) / 0.15)
    };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

fn get_room_temp_color(temp: f64) -> String {
    let (r, g, b) = if temp <= 24.0 {
        (0, 200, 0)
    } else if temp <= 25.0 {
        lerp_color((0, 200, 0), (255, 255, 0), (temp - 24.0) / 1.0)
    } else if temp <= 30.0 {
        lerp_color((255, 255, 0), (255, 165, 0), (temp - 25.0) / 5.0)
    } else if temp <= 35.0 {
        lerp_color((255, 165, 0), (255, 100, 100), (temp - 30.0) / 5.0)
    } else {
        lerp_color(
            (255, 100, 100),
            (139, 0, 0),
            ((temp - 35.0) / 5.0).clamp(0.0, 1.0),
        )
    };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

fn get_ratio_color(ratio: f64) -> String {
    let (r, g, b) = if ratio < 1.0 {
        (255, 50, 0)
    } else if ratio <= 1.5 {
        lerp_color((255, 50, 0), (255, 165, 0), (ratio - 1.0) / 0.5)
    } else if ratio <= 2.5 {
        lerp_color((255, 165, 0), (255, 255, 0), (ratio - 1.5) / 1.0)
    } else if ratio <= 4.0 {
        lerp_color((255, 255, 0), (0, 200, 0), (ratio - 2.5) / 1.5)
    } else {
        (0, 200, 0)
    };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

fn is_virtual_machine() -> bool {
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
        if cpuinfo.lines().any(|l| l.starts_with("flags") && l.contains("hypervisor")) {
            return true;
        }
    }
    if let Ok(prod) = fs::read_to_string("/sys/class/dmi/id/product_name") {
        let p = prod.to_lowercase();
        if p.contains("virtualbox") || p.contains("vmware") || p.contains("kvm") || p.contains("qemu") {
            return true;
        }
    }
    false
}

fn get_thermal_stats(is_vm: bool) -> String {
    let mut parts = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = fs::read_to_string(path.join("name")).unwrap_or_default();
            if !name.trim().contains("k10temp") && !name.trim().contains("coretemp") {
                continue;
            }
            for file in fs::read_dir(&path).into_iter().flatten().flatten() {
                let fname = file.file_name().to_string_lossy().into_owned();
                if !fname.starts_with("temp") || !fname.ends_with("_input") {
                    continue;
                }
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
                let color = get_mem_color(input_val / limit);
                let label = fs::read_to_string(path.join(fname.replace("_input", "_label")))
                    .unwrap_or_else(|_| fname.replace("_input", ""));
                parts.push(format!(
                    "{}: {}{:.1}°C{}",
                    label.trim(),
                    color,
                    input_val / 1000.0,
                    "\x1b[0m"
                ));
            }
        }
    }
    if parts.is_empty() {
        if is_vm {
            "\x1b[38;2;255;165;0mN/A (VM Guest)\x1b[0m".into()
        } else {
            "\x1b[38;2;255;0;0mN/A\x1b[0m".into()
        }
    } else {
        parts.join(" | ")
    }
}

fn get_dashed_line(max_w: usize, freqs_empty: bool, is_vm: bool) -> String {
    let mut line_text = "-".repeat(max_w);
    if freqs_empty {
        let msg = " Error: Core Frequencies cannot be accessed ";
        if msg.len() < max_w {
            let dashes = (max_w - msg.len()) / 2;
            line_text = format!(
                "{}{}{}",
                "-".repeat(dashes),
                msg,
                "-".repeat(max_w - dashes - msg.len())
            );
        }
        format!("\x1b[38;2;255;0;0m{}\x1b[0m", line_text)
    } else if is_vm {
        let msg = " VM Guest Detected ";
        if msg.len() < max_w {
            let dashes = (max_w - msg.len()) / 2;
            line_text = format!(
                "{}{}{}",
                "-".repeat(dashes),
                msg,
                "-".repeat(max_w - dashes - msg.len())
            );
        }
        format!("\x1b[38;2;255;165;0m{}\x1b[0m", line_text)
    } else {
        line_text
    }
}

fn find_temper_poll() -> Option<std::path::PathBuf> {
    // 1. Let the 'which' crate look through the active PATH
    if let Ok(path) = which::which("temper-poll") {
        return Some(path);
    }

    // 2. Build a list of fallback candidates
    let mut candidates = vec![
        std::path::PathBuf::from("/usr/local/bin/temper-poll"),
        std::path::PathBuf::from("/usr/bin/temper-poll"),
        std::path::PathBuf::from("/bin/temper-poll"),
        std::path::PathBuf::from("/opt/bin/temper-poll"),
    ];

    // 3. Address sudo execution
    // If run under sudo, $HOME becomes /root. Check the SUDO_USER variable
    // to find the original user's true home directory.
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        // Standard Linux user home directory fallback
        candidates.push(std::path::PathBuf::from(format!("/home/{}/.local/bin/temper-poll", sudo_user)));
    }

    // Normal $HOME fallback (catches cases where it's not sudo, but $PATH is somehow stripped)
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(std::path::PathBuf::from(home).join(".local/bin/temper-poll"));
    }

    // Evaluate all candidates
    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1).peekable();
    let mut cpu_interval = DEFAULT_INTERVAL;
    let mut room_interval = DEFAULT_INTERVAL;
    let mut mem_interval = DEFAULT_INTERVAL;

    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" {
            print_help();
            return Ok(());
        } else if arg.starts_with("-n") {
            let val = if arg.len() > 2 {
                arg[2..].parse().unwrap_or(DEFAULT_INTERVAL)
            } else {
                args.next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_INTERVAL)
            };
            cpu_interval = val.clamp(0.1, 60.0);
        } else if arg.starts_with("-r") {
            let val = if arg.len() > 2 {
                arg[2..].parse().unwrap_or(DEFAULT_INTERVAL)
            } else {
                args.next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_INTERVAL)
            };
            room_interval = val.clamp(1.0, 3600.0);
        } else if arg.starts_with("-m") {
            let val = if arg.len() > 2 {
                arg[2..].parse().unwrap_or(DEFAULT_INTERVAL)
            } else {
                args.next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_INTERVAL)
            };
            mem_interval = val.clamp(1.0, 60.0);
        }
    }

    let is_vm = is_virtual_machine();

    let cpu_model = fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_string())
        .unwrap_or("Unknown".into());

    let limits = (
        fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_min_freq")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(|k| k / 1000.0),
        fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(|k| k / 1000.0),
    );

    let (tx, rx) = mpsc::channel::<Msg>();

    // CPU Thread
    let tx_cpu = tx.clone();
    thread::spawn(move || loop {
        let info = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let freqs = info
            .lines()
            .filter(|l| l.starts_with("cpu MHz"))
            .filter_map(|l| l.split(':').nth(1)?.trim().parse::<f64>().ok())
            .collect();
        let _ = tx_cpu.send(Msg::CpuFreqs(freqs));
        thread::sleep(Duration::from_secs_f64(cpu_interval));
    });

    // Thermal Thread
    let tx_ctemp = tx.clone();
    thread::spawn(move || loop {
        let _ = tx_ctemp.send(Msg::CpuTemps(get_thermal_stats(is_vm)));
        thread::sleep(Duration::from_secs_f64(cpu_interval));
    });

    // Room Temp Thread
    let tx_room = tx.clone();
    thread::spawn(move || loop {
        // Resolve path dynamically before execution
        let cmd_path = find_temper_poll().unwrap_or_else(|| std::path::PathBuf::from("temper-poll"));
        
        let out = Command::new(&cmd_path).output();
        let msg = if let Ok(o) = out {
            let s = String::from_utf8_lossy(&o.stdout);
            if let Some(line) = s.lines().find(|l| l.contains("Device #0:")) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(temp_str) = parts.iter().find(|p| p.contains('°')) {
                    let clean_temp = temp_str.replace('°', "").replace('C', "");
                    if let Ok(temp) = clean_temp.parse::<f64>() {
                        format!("{}{:.1}°C{}", get_room_temp_color(temp), temp, "\x1b[0m")
                    } else {
                        "\x1b[38;2;255;0;0mNo thermometer detected\x1b[0m".to_string()
                    }
                } else {
                    "\x1b[38;2;255;0;0mNo thermometer detected\x1b[0m".to_string()
                }
            } else {
                "\x1b[38;2;255;0;0mNo thermometer detected\x1b[0m".to_string()
            }
        } else {
            "\x1b[38;2;255;0;0mNo thermometer detected\x1b[0m".to_string()
        };
        let _ = tx_room.send(Msg::RoomTemp(msg));
        thread::sleep(Duration::from_secs_f64(room_interval));
    });

    // Mem Thread
    let tx_mem = tx.clone();
    thread::spawn(move || loop {
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let (mut total, mut avail) = (0u64, 0u64);
        for line in meminfo.lines() {
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() < 2 {
                continue;
            }
            let val = p[1].parse::<u64>().unwrap_or(0);
            if p[0] == "MemTotal:" {
                total = val;
            } else if p[0] == "MemAvailable:" {
                avail = val;
            }
        }
        let used = total.saturating_sub(avail);
        let ram_percent = if total > 0 { used as f64 / total as f64 } else { 0.0 };
        let mem_color = get_mem_color(ram_percent);
        let ram_str = format!(
            "RAM: {}{}{} Used / {} Total | Avail: {}{}{}",
            mem_color,
            format_size(used),
            "\x1b[0m",
            format_size(total),
            mem_color,
            format_size(avail),
            "\x1b[0m"
        );

        let swaps = fs::read_to_string("/proc/swaps").unwrap_or_default();
        let mut total_swap = 0u64;
        let mut swap_lines = Vec::new();
        for line in swaps.lines().skip(1) {
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() >= 4 {
                let size = p[2].parse::<u64>().unwrap_or(0);
                let used = p[3].parse::<u64>().unwrap_or(0);
                total_swap += size;
                let swap_percent = if size > 0 { used as f64 / size as f64 } else { 0.0 };
                swap_lines.push(format!(
                    "{}{} ({} used / {} total){}",
                    p[0].split('/').last().unwrap_or("swap"),
                    get_mem_color(swap_percent),
                    format_size(used),
                    format_size(size),
                    "\x1b[0m"
                ));
            }
        }

        let zswap_param_path = std::path::Path::new("/sys/module/zswap/parameters/enabled");
        let zswap_str = if !zswap_param_path.exists() {
            format!("Zswap: \x1b[38;2;255;0;0mNot Present\x1b[0m")
        } else {
            match fs::read_to_string(zswap_param_path) {
                Ok(val) => match val.trim() {
                    "Y" => {
                        match (
                            fs::read_to_string("/sys/kernel/debug/zswap/pool_total_size"),
                            fs::read_to_string("/sys/kernel/debug/zswap/stored_pages"),
                        ) {
                            (Ok(p_str), Ok(pg_str)) => {
                                let pool = p_str.trim().parse::<u64>().unwrap_or(0);
                                let pages = pg_str.trim().parse::<u64>().unwrap_or(0);
                                let ratio = if pool > 0 {
                                    (pages * 4 * 1024) as f64 / (pool as f64)
                                } else {
                                    0.0
                                };
                                let pool_color = if pool > 0 {
                                    "\x1b[38;2;0;200;0m"
                                } else {
                                    "\x1b[38;2;150;150;150m"
                                };
                                let ratio_color = if ratio > 0.0 {
                                    get_ratio_color(ratio)
                                } else {
                                    "\x1b[0m".to_string()
                                };
                                format!(
                                    "Zswap: \x1b[38;2;0;200;0mEnabled\x1b[0m | Pool: {}{}{} | Ratio: {}{:.1}:1\x1b[0m",
                                    pool_color,
                                    format_size(pool / 1024),
                                    "\x1b[0m",
                                    ratio_color,
                                    ratio
                                )
                            }
                            _ => format!("Zswap: \x1b[38;2;0;200;0mEnabled\x1b[0m (\x1b[38;2;255;255;0mRequires sudo for stats\x1b[0m)"),
                        }
                    }
                    "N" => format!("Zswap: \x1b[38;2;255;0;0mDisabled\x1b[0m"),
                    _ => format!("Zswap: \x1b[38;2;255;0;0mUnknown\x1b[0m"),
                },
                Err(_) => format!("Zswap: \x1b[38;2;255;0;0mUnknown\x1b[0m"),
            }
        };

        let mut full_mem_str = format!("{}\n{}\nSwap: Total {}", ram_str, zswap_str, format_size(total_swap));
        for chunk in swap_lines.chunks(2) {
            full_mem_str.push_str(&format!("\n{}", chunk.join(" | ")));
        }
        let _ = tx_mem.send(Msg::MemStats(full_mem_str));
        thread::sleep(Duration::from_secs_f64(mem_interval));
    });

    // --- Terminal Setup ---
    terminal::enable_raw_mode()?;
    let mut stdout = BufWriter::new(io::stdout());
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    stdout.queue(terminal::Clear(ClearType::All))?;

    let mut state = SystemState {
        cpu_model,
        limits,
        freqs: vec![],
        cpu_temps: "Loading...".into(),
        room_temp: "Loading...".into(),
        mem_stats: "Loading...".into(),
    };

    loop {
        if event::poll(Duration::from_millis(10))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q')
                    || key.code == KeyCode::Char('Q')
                    || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
            }
        }

        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::CpuFreqs(f) => state.freqs = f,
                Msg::CpuTemps(t) => state.cpu_temps = t,
                Msg::RoomTemp(r) => state.room_temp = r,
                Msg::MemStats(m) => state.mem_stats = m,
            }
        }

        let (max_w, max_h) = terminal::size()?;
        let cols = (max_w as usize / MIN_CELL_WIDTH)
            .max(1)
            .min(state.freqs.len().max(1));
        let cpu_rows = (state.freqs.len() + cols - 1) / cols;
        let mem_rows = state.mem_stats.lines().count() as u16;
        let required_height = 6 + cpu_rows as u16 + 1 + mem_rows;

        if max_w < MIN_CELL_WIDTH as u16 || max_h < required_height {
            stdout.queue(terminal::Clear(ClearType::All))?;
            stdout.queue(cursor::MoveTo(0, 0))?;
            write!(stdout, "Terminal too small!")?;
            stdout.flush()?;
            continue;
        }

        stdout.queue(cursor::MoveTo(0, 0))?;
        let mut row = 0;

        let print_line = |row: &mut u16, text: String, stdout: &mut BufWriter<io::Stdout>| -> io::Result<()> {
            if *row < max_h - 1 {
                stdout.queue(cursor::MoveTo(0, *row))?;
                write!(stdout, "{}", text)?;
                stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
                *row += 1;
            }
            Ok(())
        };

        let version = env!("CARGO_PKG_VERSION");
        print_line(
            &mut row,
            format!("\x1b[1mCPU-Grid ver:{}\x1b[0m", version),
            &mut stdout,
        )?;
        print_line(&mut row, format!("\x1b[1m{}\x1b[0m", state.cpu_model), &mut stdout)?;

        match state.limits {
            (Some(min), Some(max)) => print_line(
                &mut row,
                format!("Hardware Limits: {:.0} MHz Min | {:.0} MHz Max", min, max),
                &mut stdout,
            )?,
            _ => {
                let msg = if is_vm {
                    "Hardware Limits: \x1b[38;2;255;165;0mVM detected, limits not exposed\x1b[0m"
                } else {
                    "Hardware Limits: \x1b[38;2;255;0;0mUnavailable\x1b[0m"
                };
                print_line(&mut row, msg.to_string(), &mut stdout)?;
            }
        }
        print_line(
            &mut row,
            format!("Room Temp: {} | CPU Temps: {}", state.room_temp, state.cpu_temps),
            &mut stdout,
        )?;
        print_line(
            &mut row,
            "Type 'Q' or Ctrl+C to quit.".to_string(),
            &mut stdout,
        )?;

        // Top dashed line before Core grid
        if row < max_h - 1 {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, state.freqs.is_empty(), is_vm))?;
            row += 1;
        }

        for r in 0..cpu_rows {
            if row >= max_h - 1 {
                break;
            }
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            for c in 0..cols {
                let idx = r * cols + c;
                if idx < state.freqs.len() {
                    let freq = state.freqs[idx];
                    let color = if let (Some(min), Some(max)) = state.limits {
                        get_cpu_color((freq - min) / (max - min))
                    } else {
                        String::new()
                    };
                    let (display_freq, unit) = if freq >= 1000.0 {
                        (freq / 1000.0, "GHz")
                    } else {
                        (freq, "MHz")
                    };
                    let sep = if c < cols - 1 { " | " } else { "" };
                    write!(
                        stdout,
                        "Core {:02}: {}{:8.3} {}\x1b[0m{}",
                        idx, color, display_freq, unit, sep
                    )?;
                }
            }
            row += 1;
        }

        // Bottom dashed line after Core grid
        if row < max_h - 1 {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", get_dashed_line(max_w as usize, state.freqs.is_empty(), is_vm))?;
            row += 1;
        }

        for line in state.mem_stats.lines() {
            print_line(&mut row, line.to_string(), &mut stdout)?;
        }

        stdout.queue(terminal::Clear(ClearType::FromCursorDown))?;
        stdout.flush()?;
        thread::sleep(Duration::from_millis(50));
    }

    terminal::disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
    Ok(())
}
