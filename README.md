# CPU-Grid

![CPU-Grid in action](Screenshot-Main.png)

CPU-Grid is a real-time, terminal-based system monitoring tool written in Rust. It provides a clean, color-coded overview of your system's performance, including CPU frequencies, hardware temperatures, memory utilization, and Zswap metrics.

## Features

- **Real-time Monitoring**: Tracks per-core CPU frequency, RAM/Swap usage, and hardware thermals.
- **Smart Color-Coding**: Uses dynamic color scaling to visually represent load and temperature intensity.
- **Zswap Insight**: Monitors Zswap compression and pool statistics (if enabled).
- **Room Temperature**: Integrates with `temper-poll` to display ambient room temperature.
- **Low Latency**: Built with a multi-threaded architecture for responsive updates.

## Color Legend
Color shade gradually changes between the ranges defined underneath.

| Metric | Color Scale |
| :--- | :--- |
| **CPU Freq** | Green (0-50%) → Yellow (50-70%) → Orange (70-85%) → Red (85-100%) → Violet (>100%) |
| **RAM/Swap** | Green (0-50%) → Yellow (50-70%) → Orange (70-85%) → Red (85-100%) |
| **CPU Temp** | Green (Cool) → Red (Thermal Throttle/Critical Limit) |
| **Room Temp**| Green (≤24°C) → Yellow (25°C) → Orange (30°C) → LtRed (35°C) → DkRed (40°C) |
| **Zswap** | Red (1:1) -> Orange (1.5:1) -> Yellow (2.5:1) -> Green (4:1+) |

## Requirements

- **OS**: Linux (Requires access to `/proc` and `/sys/class/hwmon` directories).
- **Rust/Cargo**: Required to build from source.
- **Dependencies**: `Crossterm`.
- **External**: `temper-poll` must be installed and in your system PATH for ambient room temperature.

## Installation

1. Clone the repository:
   ```bash
   git clone [https://github.com/StatusCode404/CPU-Grid.git](https://github.com/StatusCode404/CPU-Grid.git)
   cd cpu-grid
   ```

2. Build the project:
   ```bash
   cargo build --release
   ```

3. Run the application:
   ```bash
   ./target/release/cpu_grid
   ```

## Usage
Values are in seconds. Parameters given less than or greater than the boundary ranges will fall back to the nearest boundary range.

| Flag | Description | Default |
| :--- | :--- | :--- |
| `-n <secs>` | Interval for CPU stats | 0.1 - 60s, default 2.0 |
| `-r <secs>` | Interval for Room Temp | 1 - 3600s, default 2.0 |
| `-m <secs>` | Interval for Memory stats | 1 - 60s, default 2.0 |

**Controls:**
- Press `Q` or `Ctrl+C` to quit the application.

## License

Distributed under the GNU General Public License v3.0. See the `LICENSE` file for more information.
