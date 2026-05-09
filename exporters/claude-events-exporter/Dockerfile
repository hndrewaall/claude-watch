FROM python:3.13-slim-bookworm

COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv
RUN uv pip install --system --no-cache prometheus_client

COPY claude_events_exporter.py /app/
WORKDIR /app

EXPOSE 9103

CMD ["python3", "claude_events_exporter.py"]
