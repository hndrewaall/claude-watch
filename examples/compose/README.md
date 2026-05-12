# Fresh-laptop developer stack

End-to-end `docker compose` example that wires:

- [**claude-container**](https://github.com/hndrewaall/claude-container) (currently private — see [Caveats](#caveats)) — Claude Code + `claude-watch` + tmux baked into one image.
- **queue-minisite** (this repo, under `queue-minisite/`) — mobile-friendly Flask UI for the `session-task` work queue.
- [**eichi**](https://github.com/hndrewaall/eichi) `search-minisite` — Flask UI for the local sqlite-vec + sentence-transformers semantic-search CLI.

Drop into a freshly cloned setup, run one command, get the integrated experience: a containerised Claude Code shell, a web UI for its work queue at `http://localhost:8000/`, and a semantic-search web UI at `http://localhost:8001/`.

## Prerequisites

- Docker Engine (Linux) or Docker Desktop (macOS / Windows). Compose v2 (the `docker compose` subcommand, not legacy `docker-compose`).
- `git`.
- An Anthropic API key (`ANTHROPIC_API_KEY`) if you want the Claude Code service to actually talk to the API.
- A host UID of `1000` is the smoothest path because the container images bake the `hndrewaall` user at uid 1000. Other UIDs work but you'll see permission warnings on bind-mounted state until you adjust the `user:` directives.

## Sibling-repo layout

The compose file uses sibling-repo build contexts (`../../../eichi`, `../../../claude-container`). Clone all three repos next to each other:

```sh
mkdir -p ~/code && cd ~/code
git clone https://github.com/hndrewaall/claude-watch.git
git clone https://github.com/hndrewaall/eichi.git
# If you have access — see "Caveats" below.
git clone <claude-container-url> claude-container
```

Resulting layout:

```
~/code/
  claude-watch/
    examples/compose/   <- you run docker compose from here
  eichi/
  claude-container/     (optional)
```

Any parent directory works (`~/code/`, `~/src/`, `/srv/`, etc.) — only the sibling relationship matters.

## Pre-flight (host side)

The compose stack expects a few host directories + files to exist; the easiest way to seed them is to run each tool natively once before the first `docker compose up`.

1. **claude-watch / session-task state** — exists under `~/.claude/`, `~/.config/session/`, `~/.config/claude/`, `~/claude-events/`. Created automatically by claude-watch / session-task on first use. If you've never run claude-watch on this host, run `tools/session-task/session-task list` from the repo root once — it bootstraps `~/.config/session/queue.json`.
2. **eichi state** — exists under `~/.local/share/eichi/` (index DB) and `~/.cache/huggingface/` (embedding-model cache). `cd ~/code/eichi && uv venv --python 3.11 && uv pip install -e . && eichi index ~/Documents/notes` (or whatever you want to search) gets both. Skip if you only want the queue UI.

## Run

From `claude-watch/examples/compose/`:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
docker compose up
```

The first build will take a while (Rust + sentence-transformers wheels + Node 20 + the multi-stage claude-watch builder). Subsequent runs are cached.

Once up:

- **http://localhost:8000/** — queue-minisite UI. Lists pending / running / blocked queue items, exposes Stop / Abandon / Force-start buttons.
- **http://localhost:8001/** — eichi search UI. Type a natural-language query, get top-K hits across whatever you've indexed.

## Use the Claude Code shell

The `claude-container` service runs in the foreground by default. To drop into the in-container tmux session:

```sh
docker compose exec claude-container bash
# inside the container:
claude
```

Or use the standalone `claude-tmux` wrapper that ships in the `claude-container` repo — it's a more ergonomic entrypoint than `docker compose exec` for interactive use. See the claude-container README for details.

## First-run indexing (eichi)

The `eichi-search` container will start fine with an empty index but return zero results. Populate it from the host:

```sh
cd ~/code/eichi
uv venv --python 3.11
uv pip install -e .
eichi index ~/Documents/notes        # any directory you want searchable
eichi index ~/.claude/projects       # agent transcripts (a useful corpus)
```

The container will see the updated index next request — no restart required (sqlite-vec re-reads on connect).

## Caveats

### Container username is hardcoded

In-container paths (right-hand side of `volumes:`) hardcode `/home/hndrewaall/...` because the `hndrewaall` user is baked into the `claude-container` and `eichi-search` Dockerfiles at uid 1000. Your **host** user can be anything — bind-mount left-hand sides use `${HOME}` interpolation. If your host UID is not 1000, the bind-mounted state directories will look root-owned to the container; the cleanest fix is to add `user: "$(id -u):$(id -g)"` to each service and `chown` the host directories before launching.

### claude-container is currently private

The `claude-container` repo isn't public yet (as of this writing). If you don't have access:

1. Comment out the entire `claude-container:` service block in `docker-compose.yml`.
2. Run `docker compose up queue-minisite eichi-search` instead.

The queue UI and search UI work standalone. You'd then run `claude` natively (or via your own wrapper) on the host.

### queue-minisite Dockerfile dependency

This compose file references `queue-minisite/Dockerfile` and `tools/session-task/session-task` from the claude-watch repo root. Both land via [PR #100](https://github.com/hndrewaall/claude-watch/pull/100) ("Add queue-minisite (Flask UI for session-task queue)"). If you're on a branch that predates that merge, check out `feat/absorb-queue-minisite` or wait for it to land on `main`.

### No upstream auth gate

`queue-minisite` and `eichi-search` are designed to sit BEHIND an authentication proxy (oauth2-proxy, nginx `auth_request`, etc.). The included compose binds them directly to `localhost:8000` / `localhost:8001` with no gate — fine for local single-user dev, NOT fine for exposure on a public IP. Don't `-p 0.0.0.0:8000:8000` this without an auth layer in front.

## Tear down

```sh
docker compose down              # stop + remove containers (volumes survive)
docker compose down -v           # also nuke the claude-container-versions volume
```

The bind-mounted host state under `~/.claude`, `~/.config/session`, `~/.local/share/eichi`, etc. is untouched by `down` — only named volumes go.
