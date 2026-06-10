#!/bin/bash
LOG=/Users/hallandrew/repos/claude-watch/redeploy-test.log
CD=/Users/hallandrew/repos/claude-watch/examples/compose

exec > "$LOG" 2>&1
set -x

echo "=== PRE-REDEPLOY STATE ==="
date -u
docker compose -f "$CD/docker-compose.yml" ps claude-container
docker compose -f "$CD/docker-compose.yml" exec claude-container ps -eo pid,ppid,user,cmd --sort=pid 2>/dev/null

echo "=== STARTING FORCE-RECREATE ==="
date -u
START=$(date +%s)
docker compose -f "$CD/docker-compose.yml" up -d --force-recreate claude-container
RC=$?
END=$(date +%s)
echo "=== FORCE-RECREATE DONE rc=$RC elapsed=$((END-START))s ==="
date -u

echo "=== POST-REDEPLOY STATE ==="
sleep 10
docker compose -f "$CD/docker-compose.yml" ps claude-container
docker compose -f "$CD/docker-compose.yml" exec claude-container ps -eo pid,ppid,user,cmd --sort=pid 2>/dev/null

echo "=== DONE ==="
