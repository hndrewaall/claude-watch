FROM python:3.13-slim-bookworm

COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN uv pip install --system --no-cache prometheus_client

COPY work_queue_exporter.py /app/
# Shared agent-id / agent-state helper. Lives alongside the exporter
# in claude-watch/exporters/work-queue-exporter/. Originally vendored
# from claude-config/lib/claude_agents.py; now maintained here.
COPY claude_agents.py /app/
WORKDIR /app

EXPOSE 9099

CMD ["python3", "work_queue_exporter.py"]
