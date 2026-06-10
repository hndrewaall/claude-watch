#!/bin/bash
# test-redeploy-cw.sh — simulates cw --up and logs what happens
LOG=/Users/hallandrew/repos/claude-watch/redeploy-cw-test.log
CD=/Users/hallandrew/repos/claude-watch/examples/compose
SERVICE=claude-container
SESSION=claude-container

exec > "$LOG" 2>&1
set -x

echo "=== STEP 1: docker compose up -d ==="
date -u
docker compose -f "$CD/docker-compose.yml" up -d --force-recreate $SERVICE
echo "up -d rc=$?"
date -u

echo "=== STEP 2: attach loop ==="
for i in $(seq 1 15); do
    echo "--- attempt $i ---"
    date -u
    docker compose -f "$CD/docker-compose.yml" exec $SERVICE tmux has-session -t $SESSION 2>&1
    HAS_RC=$?
    echo "has-session rc=$HAS_RC"

    if [ "$HAS_RC" = "0" ]; then
        echo "SESSION EXISTS"
        docker compose -f "$CD/docker-compose.yml" exec $SERVICE tmux capture-pane -t $SESSION -p 2>&1 | tail -10
        break
    fi
    sleep 1
done

echo "=== STEP 3: final state ==="
docker compose -f "$CD/docker-compose.yml" ps $SERVICE
docker compose -f "$CD/docker-compose.yml" exec $SERVICE ps -eo pid,ppid,user,cmd --sort=pid 2>/dev/null | head -15
echo "=== DONE ==="
