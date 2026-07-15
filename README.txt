================================================================================
                                 CPU-Grid
================================================================================

CPU-Grid is a real-time, terminal-based system monitoring tool written in Rust. 
It provides a clean, color-coded overview of your system's performance, 
including CPU frequencies, hardware temperatures, memory utilization, and 
Zswap metrics.

--------------------------------------------------------------------------------
FEATURES
--------------------------------------------------------------------------------
>>> Real-time Monitoring: Tracks per-core CPU frequency, RAM/Swap usage, and 
    hardware thermals.
>>> Smart Color-Coding: Uses dynamic color scaling to visually represent load 
    and temperature intensity.
>>> Zswap Insight: Monitors Zswap compression and pool statistics (if enabled).
>>> Room Temperature: Integrates with temper-poll to display ambient room 
    temperature.
>>> Low Latency: Built with a multi-threaded architecture for responsive updates.

--------------------------------------------------------------------------------
REQUIREMENTS
--------------------------------------------------------------------------------
* OS: Linux (Requires access to /proc and /sys/class/hwmon directories).
* Dependencies: Crossterm (Used for terminal manipulation).
* External Command: Requires temper-poll to be installed and in your system 
  PATH to display room temperature.

--------------------------------------------------------------------------------
INSTALLATION
--------------------------------------------------------------------------------
1. Clone the repository:
   git clone https://github.com/StatusCode404/CPU-Grid.git
   cd cpu-grid

2. Build the project:
   cargo build --release

3. Run the application:
   ./target/release/cpu-grid

--------------------------------------------------------------------------------
USAGE
--------------------------------------------------------------------------------
The application accepts the following command-line arguments to customize 
refresh intervals:

| Flag        | Description                 | Default |
|-------------|-----------------------------|---------|
| -n <secs>   | Interval for CPU stats      | 2.0s    |
| -r <secs>   | Interval for Room Temp      | 2.0s    |
| -m <secs>   | Interval for Memory stats   | 2.0s    |

Controls:
>>> Press 'Q' or Ctrl+C to quit the application.

--------------------------------------------------------------------------------
LICENSE
--------------------------------------------------------------------------------
Distributed under the GNU General Public License v3.0. See the LICENSE file 
for more information.
================================================================================
