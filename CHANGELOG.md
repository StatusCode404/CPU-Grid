================================================================================
CPU-Grid v1.0.0

This is the initial stable release (1.0.0) of CPU-Grid, a real-time,
terminal-based system monitoring tool.

--- FEATURES ---

    Real-time Monitoring: Track per-core CPU frequency, RAM/Swap usage,
    and hardware thermals.

    Smart Color-Coding: Dynamic visual feedback to represent CPU frequency
    overclocking, load intensity, and temperature thresholds.

    Zswap Insight: Monitor compression and pool statistics.

    Room Temperature: Integration with temper-poll for ambient temperature tracking.

    High Performance: Multi-threaded architecture for low-latency updates.

--- RELEASE NOTES ---
This version stabilizes the core feature set.

    CPU Frequencies now utilize dynamic coloring to indicate load and
    overclocking (Green to Violet).

    Thermal monitoring includes dynamic detection of critical thresholds provided
    by system hardware.

    Command-line arguments allow for custom refresh intervals for CPU, RAM,
    and Room Temp metrics.

For installation instructions and usage, please see the README.txt.

================================================================================
