use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
    QueueableCommand,
};
use std::env;
use std::fs;
use std::io::{self, Write, BufWriter};
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
    println!("\x1b[1mCPU-Grid ver:{}\x1b[0m", version);
    println!("Copyright (C) 2026 StatusCode404 https://github.com/StatusCode404");
    println!("Project: https://github.com/StatusCode404/CPU-Grid");
    println!("\nUsage (Values are in seconds. Parameters given less than or greater than the boundary ranges will fall back to the nearest boundary range.):");
    println!("  -n[<secs>]    Interval for CPU stats (0.1 - 60s, default 2.0)");
    println!("  -r[<secs>]    Interval for Room Temp (1 - 3600s, default 2.0)");
    println!("  -m[<secs>]    Interval for Memory stats (1 - 60s, default 2.0)");
    println!("\nColor Legend (Color shade gradually changes between the ranges defined underneath):");
    println!("  CPU Freq:      Green(0-50%) -> Yellow(50-70%) -> Orange(70-85%) -> Red(85-100%) -> Violet(>100% overclock)");
    println!("  RAM/Swap Load: Green(0-50%) -> Yellow(50-70%) -> Orange(70-85%) -> Red(85-100%)");
    println!("  CPU Temp:      Green (Cool) -> Red (Thermal Throttle/Critical Limit)");
    println!("                 (Note: Red limit is dynamic, set by your specific CPU hardware)");
    println!("  Room Temp:     Green (<=24) -> Yellow(25) -> Orange(30) -> LtRed(35) -> DkRed(40)");
    println!("  Zswap Ratio:   Red (1:1) -> Orange (1.5:1) -> Yellow (2.5:1) -> Green (4:1+)");
}

fn format_size(kb: u64) -> String {
    if kb < 1024 { format!("{} KB", kb) }
    else if kb < 1024 * 1024 { format!("{:.1} MB", kb as f64 / 1024.0) }
    else { format!("{:.2} GB", kb as f64 / (1024.0 * 1024.0)) }
}

fn lerp_color(c1: (u8, u8, u8), c2: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    ((c1.0 as f64 + (c2.0 as f64 - c1.0 as f64) * t).round() as u8,
     (c1.1 as f64 + (c2.1 as f64 - c1.1 as f64) * t).round() as u8,
     (c1.2 as f64 + (c2.2 as f64 - c1.2 as f64) * t).round() as u8)
}

fn get_cpu_color(t: f64) -> String {
    if t > 1.0 { return "\x1b[38;2;238;130;238m".to_string(); }
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t <= 0.5 { lerp_color((0, 200, 0), (255, 255, 0), t / 0.5) }
    else if t <= 0.7 { lerp_color((255, 255, 0), (255, 165, 0), (t - 0.5) / 0.2) }
    else if t <= 0.85 { lerp_color((255, 165, 0), (255, 50, 0), (t - 0.7) / 0.15) }
    else { lerp_color((255, 50, 0), (139, 0, 0), (t - 0.85) / 0.15) };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

fn get_mem_color(t: f64) -> String {
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t <= 0.5 { lerp_color((0, 200, 0), (255, 255, 0), t / 0.5) }
    else if t <= 0.7 { lerp_color((255, 255, 0), (255, 165, 0), (t - 0.5) / 0.2) }
    else if t <= 0.85 { lerp_color((255, 165, 0), (255, 50, 0), (t - 0.7) / 0.15) }
    else { lerp_color((255, 50, 0), (139, 0, 0), (t - 0.85) / 0.15) };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

fn get_room_temp_color(temp: f64) -> String {
    let (r, g, b) = if temp <= 24.0 { (0, 200, 0) }
    else if temp <= 25.0 { lerp_color((0, 200, 0), (255, 255, 0), (temp - 24.0) / 1.0) }
    else if temp <= 30.0 { lerp_color((255, 255, 0), (255, 165, 0), (temp - 25.0) / 5.0) }
    else if temp <= 35.0 { lerp_color((255, 165, 0), (255, 100, 100), (temp - 30.0) / 5.0) }
    else { lerp_color((255, 100, 100), (139, 0, 0), ((temp - 35.0) / 5.0).clamp(0.0, 1.0)) };
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

fn get_ratio_color(ratio: f64) -> String {
    let (r, g, b) = if ratio < 1.0 { (255, 50, 0) } // Red
    else if ratio <= 1.5 { lerp_color((255, 50, 0), (255, 165, 0), (ratio - 1.0) / 0.5) } // Red to Orange
    else if ratio <= 2.5 { lerp_color((255, 165, 0), (255, 255, 0), (ratio - 1.5) / 1.0) } // Orange to Yellow
    else if ratio <= 4.0 { lerp_color((255, 255, 0), (0, 200, 0), (ratio - 2.5) / 1.5) } // Yellow to Green
    else { (0, 200, 0) }; // Green 4:1+
    format!("\x1b[38;2;{};{};{}m", r, g, b)
}

fn get_thermal_stats() -> String {
    let mut parts = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = fs::read_to_string(path.join("name")).unwrap_or_default();
            if !name.trim().contains("k10temp") && !name.trim().contains("coretemp") { continue; }
            for file in fs::read_dir(&path).into_iter().flatten().flatten() {
                let fname = file.file_name().to_string_lossy().into_owned();
                if !fname.starts_with("temp") || !fname.ends_with("_input") { continue; }
                let input_val = fs::read_to_string(file.path()).ok().and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.0);
                let read_limit = |file_name| { fs::read_to_string(file.path().with_file_name(file_name)).ok().and_then(|s| s.trim().parse::<f64>().ok()) };
                let limit = read_limit(fname.replace("_input", "_max")).or_else(|| read_limit(fname.replace("_input", "_crit"))).unwrap_or(95000.0);
                let color = get_mem_color(input_val / limit);
                let label = fs::read_to_string(path.join(fname.replace("_input", "_label"))).unwrap_or_else(|_| fname.replace("_input", ""));
                parts.push(format!("{}: {}{:.1}°C{}", label.trim(), color, input_val / 1000.0, "\x1b[0m"));
            }
        }
    }
    if parts.is_empty() { "N/A".into() } else { parts.join(" | ") }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1).peekable();
    let mut cpu_interval = DEFAULT_INTERVAL;
    let mut room_interval = DEFAULT_INTERVAL;
    let mut mem_interval = DEFAULT_INTERVAL;

    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" {
            print_help(); return Ok(());
        } else if arg.starts_with("-n") {
            let val = if arg.len() > 2 { arg[2..].parse().unwrap_or(DEFAULT_INTERVAL) }
                      else { args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL) };
            cpu_interval = val.clamp(0.1, 60.0);
        } else if arg.starts_with("-r") {
            let val = if arg.len() > 2 { arg[2..].parse().unwrap_or(DEFAULT_INTERVAL) }
                      else { args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL) };
            room_interval = val.clamp(1.0, 3600.0);
        } else if arg.starts_with("-m") {
            let val = if arg.len() > 2 { arg[2..].parse().unwrap_or(DEFAULT_INTERVAL) }
                      else { args.next().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_INTERVAL) };
            mem_interval = val.clamp(1.0, 60.0);
        }
    }

    let cpu_model = fs::read_to_string("/proc/cpuinfo").unwrap_or_default()
        .lines().find(|l| l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1)).map(|s| s.trim().to_string()).unwrap_or("Unknown".into());

    let limits = (
        fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_min_freq").ok().and_then(|s| s.trim().parse::<f64>().ok()).map(|k| k/1000.0),
        fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").ok().and_then(|s| s.trim().parse::<f64>().ok()).map(|k| k/1000.0),
    );

    let (tx, rx) = mpsc::channel::<Msg>();
    
    // CPU Thread
    let tx_cpu = tx.clone(); 
    thread::spawn(move || loop { 
        let info = fs::read_to_string("/proc/cpuinfo").unwrap_or_default(); 
        let freqs = info.lines().filter(|l| l.starts_with("cpu MHz")).filter_map(|l| l.split(':').nth(1)?.trim().parse::<f64>().ok()).collect(); 
        let _ = tx_cpu.send(Msg::CpuFreqs(freqs)); 
        thread::sleep(Duration::from_secs_f64(cpu_interval)); 
    });
    
    // Thermal Thread
    let tx_ctemp = tx.clone(); 
    thread::spawn(move || loop { 
        let _ = tx_ctemp.send(Msg::CpuTemps(get_thermal_stats())); 
        thread::sleep(Duration::from_secs_f64(cpu_interval)); 
    });

    // Room Temp Thread
    let tx_room = tx.clone(); 
    thread::spawn(move || loop { 
        let out = Command::new("temper-poll").output();
        let msg = if let Ok(o) = out {
            let s = String::from_utf8_lossy(&o.stdout);
            if let Some(line) = s.lines().find(|l| l.contains("Device #0:")) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(temp_str) = parts.iter().find(|p| p.contains('°')) {
                    let clean_temp = temp_str.replace('°', "").replace('C', "");
                    if let Ok(temp) = clean_temp.parse::<f64>() {
                        format!("{}{:.1}°C{}", get_room_temp_color(temp), temp, "\x1b[0m")
                    } else { "No thermometer detected".to_string() }
                } else { "No thermometer detected".to_string() }
            } else { "No thermometer detected".to_string() }
        } else { "No thermometer detected".to_string() };
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
            if p.len() < 2 { continue; } 
            let val = p[1].parse::<u64>().unwrap_or(0); 
            if p[0] == "MemTotal:" { total = val; } else if p[0] == "MemAvailable:" { avail = val; } 
        } 
        let used = total.saturating_sub(avail); 
        let ram_percent = if total > 0 { used as f64 / total as f64 } else { 0.0 }; 
        let ram_str = format!("RAM: {}{}{} Used / {} Total | Avail: {}", get_mem_color(ram_percent), format_size(used), "\x1b[0m", format_size(total), format_size(avail)); 
        
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
                swap_lines.push(format!("{}{} ({} used / {} total){}", p[0].split('/').last().unwrap_or("swap"), get_mem_color(swap_percent), format_size(used), format_size(size), "\x1b[0m")); 
            } 
        } 
        
        let mut zswap_str = "Zswap: Disabled".to_string(); 
        if fs::read_to_string("/sys/module/zswap/parameters/enabled").unwrap_or_default().trim() == "Y" { 
            match (fs::read_to_string("/sys/kernel/debug/zswap/pool_total_size"), fs::read_to_string("/sys/kernel/debug/zswap/stored_pages")) {
                (Ok(p_str), Ok(pg_str)) => {
                    let pool = p_str.trim().parse::<u64>().unwrap_or(0); 
                    let pages = pg_str.trim().parse::<u64>().unwrap_or(0); 
                    let ratio = if pool > 0 { (pages * 4 * 1024) as f64 / (pool as f64) } else { 0.0 }; 
                    let pool_color = if pool > 0 { "\x1b[38;2;0;200;0m" } else { "\x1b[38;2;150;150;150m" }; 
                    let ratio_color = if ratio > 0.0 { get_ratio_color(ratio) } else { "\x1b[0m".to_string() }; 
                    zswap_str = format!("Zswap: Enabled | Pool: {}{}{} | Ratio: {}{:.1}:1\x1b[0m", pool_color, format_size(pool / 1024), "\x1b[0m", ratio_color, ratio); 
                },
                _ => zswap_str = "Zswap: Enabled (Error reading stats)".to_string(),
            }
        } 
        let full_mem_str = format!("{}\n{}\nSwap: Total {}\nSwap Devices: {}", ram_str, zswap_str, format_size(total_swap), swap_lines.join(" | ")); 
        let _ = tx_mem.send(Msg::MemStats(full_mem_str)); 
        thread::sleep(Duration::from_secs_f64(mem_interval)); 
    });

    // --- Terminal Setup ---
    terminal::enable_raw_mode()?;
    let mut stdout = BufWriter::new(io::stdout());
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?; 
    stdout.queue(terminal::Clear(ClearType::All))?; 

    let mut state = SystemState { cpu_model, limits, freqs: vec![], cpu_temps: "Loading...".into(), room_temp: "Loading...".into(), mem_stats: "Loading...".into() };

    loop {
        if event::poll(Duration::from_millis(10))? {
            if let Event::Key(key) = event::read()? {
                if key.code == KeyCode::Char('q') || key.code == KeyCode::Char('Q') || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)) {
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
        let cols = (max_w as usize / MIN_CELL_WIDTH).max(1).min(state.freqs.len().max(1));
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
        print_line(&mut row, format!("\x1b[1mCPU-Grid ver:{}\x1b[0m", version), &mut stdout)?;
        print_line(&mut row, format!("\x1b[1m{}\x1b[0m", state.cpu_model), &mut stdout)?;

        match state.limits {
            (Some(min), Some(max)) => print_line(&mut row, format!("Hardware Limits: {:.0} MHz Min | {:.0} MHz Max", min, max), &mut stdout)?,
            _ => print_line(&mut row, "Hardware Limits: Unavailable".to_string(), &mut stdout)?,
        }
        print_line(&mut row, format!("Room Temp: {} | CPU Temps: {}", state.room_temp, state.cpu_temps), &mut stdout)?;
        print_line(&mut row, "Type 'Q' or Ctrl+C to quit.".to_string(), &mut stdout)?;
        
        if row < max_h - 1 {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", "-".repeat(max_w as usize))?;
            row += 1;
        }

        for r in 0..cpu_rows {
            if row >= max_h - 1 { break; }
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            for c in 0..cols {
                let idx = r * cols + c;
                if idx < state.freqs.len() {
                    let freq = state.freqs[idx];
                    let color = if let (Some(min), Some(max)) = state.limits {
                        get_cpu_color((freq - min) / (max - min))
                    } else { String::new() };
                    let (display_freq, unit) = if freq >= 1000.0 { (freq / 1000.0, "GHz") } else { (freq, "MHz") };
                    let sep = if c < cols - 1 { " | " } else { "" };
                    write!(stdout, "Core {:02}: {}{:8.3} {}\x1b[0m{}", idx, color, display_freq, unit, sep)?;
                }
            }
            row += 1;
        }
        
        if row < max_h - 1 {
            stdout.queue(cursor::MoveTo(0, row))?;
            stdout.queue(terminal::Clear(ClearType::UntilNewLine))?;
            write!(stdout, "{}", "-".repeat(max_w as usize))?;
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
