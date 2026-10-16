
#!/usr/bin/env bash
set -Eeuo pipefail

# PostgreSQL starten
docker-entrypoint.sh postgres &
PG_PID=$!

# Auf PostgreSQL warten
until pg_isready -h 127.0.0.1 -p 5432 -U postgres; do
    kill -0 "$PG_PID" 2>/dev/null || exit 1
    sleep 1
done

# OpenCode intern starten
opencode serve \
    --hostname 127.0.0.1 \
    --port 4096 &
OC_PID=$!

# Auf OpenCode warten
until curl -fsS \
    http://127.0.0.1:4096/global/health \
    >/dev/null 2>&1; do
    kill -0 "$OC_PID" 2>/dev/null || exit 1
    sleep 1
done

# MCP-Server starten
(
    cd /opt/opencode-mcp
    exec node dist/index.js
) &
MCP_PID=$!

cleanup() {
    trap - EXIT TERM INT
    echo "Stopping services..."

    kill -TERM "$MCP_PID" "$OC_PID" "$PG_PID" \
        2>/dev/null || true

    wait 2>/dev/null || true
}

trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT

# Container läuft, solange alle Dienste laufen
wait -n "$PG_PID" "$OC_PID" "$MCP_PID"

