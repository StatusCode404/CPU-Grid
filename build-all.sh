#!/usr/bin/env bash
# ============================================================================
# SECTION: Script Configuration & Environment Options
# ============================================================================
# Description: Enforces strict Bash error handling and defines target paths.
# Hint: Script halts immediately on unhandled errors, missing vars, or pipe failures.
set -euo pipefail

# Output directory for release artifacts
DIST_DIR="dist"

# Project binary name (must match package name in Cargo.toml)
BIN_NAME="cpu-grid"

# ============================================================================
# SECTION: Pre-flight Verification & Automated Toolchain Setup
# ============================================================================
# Description: Validates required cross-compilation binaries (zig, cargo-zigbuild)
#              and auto-installs missing rustup standard library target triples.
# Hint: Guarantees all required cross-linker and target dependencies exist.
echo "==> Verifying cross-compilation environment..."

if ! command -v zig &> /dev/null; then
    echo "ERROR: 'zig' executable not found in PATH." >&2
    exit 1
fi

if ! command -v cargo-zigbuild &> /dev/null; then
    echo "ERROR: 'cargo-zigbuild' utility not found." >&2
    exit 1
fi

# Target Matrix Format: "TARGET_TRIPLE|TARGET_CPU|OUTPUT_SUFFIX"
BUILD_MATRIX=(
    # --- x86-64 Linux Microarchitecture Generations ---
    "x86_64-unknown-linux-gnu|x86-64|x86_64-v1-pre-avx"
    "x86_64-unknown-linux-gnu|x86-64-v2|x86_64-v2-avx"
    "x86_64-unknown-linux-gnu|x86-64-v3|x86_64-v3-avx2"
    "x86_64-unknown-linux-gnu|x86-64-v4|x86_64-v4-avx512"

    # --- Apple Silicon Hardware Running Linux (Asahi / Arch ARM) ---
    "aarch64-unknown-linux-gnu|apple-m1|asahi-linux-apple-m1"
    "aarch64-unknown-linux-gnu|apple-m2|asahi-linux-apple-m2"
    "aarch64-unknown-linux-gnu|apple-m3|asahi-linux-apple-m3"

    # --- Raspberry Pi & ARM Hardware Running Linux ---
    "arm-unknown-linux-gnueabi|arm1176jzf-s|rpi1-zero"
    "armv7-unknown-linux-gnueabihf|cortex-a7|rpi2"
    "aarch64-unknown-linux-gnu|cortex-a53|rpi3-zero2w"
    "aarch64-unknown-linux-gnu|cortex-a72|rpi4"
    "aarch64-unknown-linux-gnu|cortex-a76|rpi5"
)

# Extract and auto-install required Rust target triples via rustup
echo "==> Checking and installing required Rust target triples..."
REQUIRED_TARGETS=$(printf "%s\n" "${BUILD_MATRIX[@]}" | cut -d'|' -f1 | sort -u)
for target in $REQUIRED_TARGETS; do
    rustup target add "$target" > /dev/null 2>&1 || true
done

# ============================================================================
# SECTION: Workspace Cleanup & Directory Initialization
# ============================================================================
# Description: Cleans previous build outputs and temporary parallel target dirs.
# Hint: Flushes existing output directory prior to execution.
echo "==> Cleaning old build artifacts..."
cargo clean
rm -rf "$DIST_DIR" target/build-*
mkdir -p "$DIST_DIR" target

# ============================================================================
# SECTION: Parallel Compilation Execution
# ============================================================================
# Description: Spawns concurrent build jobs by isolating CARGO_TARGET_DIR for 
#              each target and logging output to per-target log files.
# Hint: Dumps log on failure so errors are never lost.
echo "==> Spawning parallel compilation jobs across available CPU cores..."

pids=()

for entry in "${BUILD_MATRIX[@]}"; do
    IFS="|" read -r TARGET CPU SUFFIX <<< "$entry"
    
    OUTPUT_NAME="${BIN_NAME}-${SUFFIX}"
    TARGET_DIR="target/build-${SUFFIX}"
    LOG_FILE="${TARGET_DIR}.log"

    (
        echo "[START] Building ${OUTPUT_NAME} (${TARGET} / ${CPU})"

        # Isolate CARGO_TARGET_DIR and write compiler output to log file
        if RUSTFLAGS="-A linker_messages -C target-cpu=${CPU}" \
           CARGO_TARGET_DIR="${TARGET_DIR}" \
           cargo zigbuild --release --target "${TARGET}" > "${LOG_FILE}" 2>&1; then
            
            SRC_BIN="${TARGET_DIR}/${TARGET}/release/${BIN_NAME}"
            if [ -f "$SRC_BIN" ]; then
                cp "$SRC_BIN" "${DIST_DIR}/${OUTPUT_NAME}"
                echo "[DONE]  Saved: ${DIST_DIR}/${OUTPUT_NAME}"
            else
                echo "[FAIL]  Expected binary output not found at ${SRC_BIN}" >&2
                echo "--- Error log for ${OUTPUT_NAME} ---" >&2
                cat "${LOG_FILE}" >&2
                exit 1
            fi
        else
            echo "[FAIL]  Compilation failed for ${OUTPUT_NAME}" >&2
            echo "--- Error log for ${OUTPUT_NAME} ---" >&2
            cat "${LOG_FILE}" >&2
            exit 1
        fi
    ) &

    pids+=($!)
done

# ============================================================================
# SECTION: Process Synchronization & Status Verification
# ============================================================================
# Description: Waits for all background tasks to finish and validates status codes.
# Hint: Fails immediately if any individual worker process returns a non-zero exit code.
echo "==> Parallel jobs launched! Waiting for compilation workers to finish..."

FAILURES=0
for pid in "${pids[@]}"; do
    if ! wait "$pid"; then
        FAILURES=$((FAILURES + 1))
    fi
done

if [ "$FAILURES" -ne 0 ]; then
    echo "====================================================================" >&2
    echo "ERROR: ${FAILURES} parallel build process(es) failed." >&2
    echo "====================================================================" >&2
    exit 1
fi

# Clean up temporary worker target directories and logs
rm -rf target/build-*

echo "===================================================================="
echo "Build complete! All binaries generated in parallel in ./${DIST_DIR}/"
echo "===================================================================="
