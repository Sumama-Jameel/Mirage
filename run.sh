#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$SCRIPT_DIR/target/release/obscura-gateway"
PID_FILE="$SCRIPT_DIR/.gateway.pid"
LOG="$SCRIPT_DIR/gateway.log"

start() {
    if [ -f "$PID_FILE" ]; then
        PID=$(cat "$PID_FILE")
        if kill -0 "$PID" 2>/dev/null; then
            echo "Gateway already running (PID $PID)"
            return 0
        fi
        rm -f "$PID_FILE"
    fi

    if [ ! -f "$BIN" ]; then
        echo "Binary not found at $BIN. Run: $0 build"
        exit 1
    fi

    echo "Starting obscura-gateway on http://127.0.0.1:8080 ..."
    setsid nohup "$BIN" > "$LOG" 2>&1 &
    NEW_PID=$!
    disown "$NEW_PID"
    echo "$NEW_PID" > "$PID_FILE"

    # Wait for port to be ready
    for i in $(seq 1 30); do
        if curl -s -o /dev/null -w "" --max-time 1 http://127.0.0.1:8080/v1/models 2>/dev/null; then
            echo "Gateway ready after ${i}s (PID $NEW_PID)"
            return 0
        fi
        # Check if process is still alive
        if ! kill -0 "$NEW_PID" 2>/dev/null; then
            echo "Gateway process died during startup. Check $LOG"
            tail -5 "$LOG"
            return 1
        fi
        sleep 1
    done
    echo "Gateway failed to start in 30s. Check $LOG"
    tail -5 "$LOG"
    return 1
}

stop() {
    if [ -f "$PID_FILE" ]; then
        PID=$(cat "$PID_FILE")
        if kill -0 "$PID" 2>/dev/null; then
            echo "Stopping gateway (PID $PID)..."
            kill "$PID" 2>/dev/null || true
            # Wait up to 5s for graceful shutdown
            for i in $(seq 1 5); do
                kill -0 "$PID" 2>/dev/null || break
                sleep 1
            done
            # Force kill if still alive
            kill -9 "$PID" 2>/dev/null || true
            rm -f "$PID_FILE"
            echo "Stopped"
        else
            echo "Gateway not running (stale PID file)"
            rm -f "$PID_FILE"
        fi
    else
        # Try to find by process name
        PIDS=$(pgrep -f "target/release/obscura-gateway" 2>/dev/null || true)
        if [ -n "$PIDS" ]; then
            echo "Killing orphan gateway processes: $PIDS"
            echo "$PIDS" | xargs kill -9 2>/dev/null || true
        else
            echo "No gateway process found"
        fi
    fi
}

status() {
    if [ -f "$PID_FILE" ]; then
        PID=$(cat "$PID_FILE")
        if kill -0 "$PID" 2>/dev/null; then
            echo "Running (PID $PID)"
            # Quick health check
            if curl -s -o /dev/null -w "" --max-time 2 http://127.0.0.1:8080/v1/models 2>/dev/null; then
                echo "Health: OK (port 8080 responding)"
            else
                echo "Health: WARN (process alive but port not responding)"
            fi
            return 0
        fi
        echo "Not running (stale PID file)"
        rm -f "$PID_FILE"
        return 1
    else
        echo "Not running (no PID file)"
        return 1
    fi
}

case "${1:-help}" in
    start)   start ;;
    stop)    stop ;;
    restart) stop; sleep 1; start ;;
    status)  status ;;
    log)     tail -f "$LOG" ;;
    build)
        echo "Building obscura-gateway in release mode..."
        cargo build --release -p obscura-gateway
        echo "Build complete: $BIN"
        ;;
    *)
        echo "Usage: $0 {start|stop|restart|status|log|build}"
        exit 1
        ;;
esac
