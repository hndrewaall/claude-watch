# Fresh-laptop developer stack

End-to-end `docker compose` example that wires:

- **claude-container** (this repo, under [`container/`](../../container/)) — Claude Code + `claude-watch` + tmux baked into one image.
- **queue-minisite** (this repo, under `queue-minisite/`) — mobile-friendly Flask UI for the `session-task` work queue.
- [**eichi**](https://github.com/hndrewaall/eichi) `search-minisite` — Flask UI for the local sqlite-vec + sentence-transformers semantic-search CLI.
- **ttyd** (this repo, under [`ttyd/`](ttyd/)) — browser-based terminal that attaches to the claude-container's in-container tmux session.

Drop into a freshly cloned setup, run one command, get the integrated experience: a containerised Claude Code shell, a web UI for its work queue at `http://localhost:8000/`, a semantic-search web UI at `http://localhost:8001/`, and a browser terminal at `http://localhost:7681/`.

Looking for periodic event emissions instead? See [`examples/cron/`](../cron/README.md) for a cron-driven `claude-event` example.

## Prerequisites

- Docker Engine (Linux) or Docker Desktop (macOS / Windows). Compose v2 (the `docker compose` subcommand, not legacy `docker-compose`).
- `git`.
- An Anthropic API key (`ANTHROPIC_API_KEY`) if you want the Claude Code service to actually talk to the API.
- A host UID of `1000` is the smoothest path because the container images bake the `hndrewaall` user at uid 1000. Other UIDs work but you'll see permission warnings on bind-mounted state until you adjust the `user:` directives.

## Sibling-repo layout

The compose file uses an in-repo build context (`../../container`) for the
claude-container service and a sibling-repo context (`../../../eichi`) for
eichi search. Clone both repos next to each other:

```sh
mkdir -p ~/code && cd ~/code
git clone https://github.com/hndrewaall/claude-watch.git
git clone https://github.com/hndrewaall/eichi.git
```

Resulting layout:

```
~/code/
  claude-watch/
    container/          <- claude-container source (this repo)
    examples/compose/   <- you run docker compose from here
  eichi/
```

Any parent directory works (`~/code/`, `~/src/`, `/srv/`, etc.) — only the sibling relationship between `claude-watch/` and `eichi/` matters.

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
- **http://localhost:7681/** — ttyd browser terminal. Drops you directly into the claude-container's tmux session (same view as `docker compose exec claude-container tmux attach`).

## Use the Claude Code shell

The `claude-container` service runs in the foreground by default. To drop into the in-container tmux session:

```sh
docker compose exec claude-container bash
# inside the container:
claude
```

Or use the standalone `claude-tmux` wrapper at [`container/bin/claude-tmux`](../../container/bin/claude-tmux) — it's a more ergonomic entrypoint than `docker compose exec` for interactive use. See [`container/README.md`](../../container/README.md) for details.

## ttyd web console

After `docker compose up -d`, a browser-attachable terminal is available at
**http://localhost:7681/** that drops you directly into the claude-container's
tmux session — the same view as `docker compose exec claude-container tmux
attach`. Useful when you want a Claude Code shell from a tablet / phone / second
machine on the LAN without setting up SSH.

### How it's wired

Both `claude-container` and `ttyd` share the named volume `claude-tmux-socket`
mounted at `/tmp/tmux-1000` — tmux's default socket directory for uid 1000.
The claude-container entrypoint creates the session on the default socket
(`tmux new-session -d -s claude-container ...`); ttyd then attaches via
`tmux -S /tmp/tmux-1000/default attach-session -t claude-container`. Both
services run as uid 1000 so socket permissions align.

### Cross-platform volume perms

On Docker Desktop (macOS / Windows), the bundled `tmux-socket-init` service
chowns the shared `/tmp/tmux-1000` volume to uid 1000 mode 0700 on first
start. This is necessary because Docker Desktop's volume layer does not
propagate the image-side directory perms the way native Linux Docker does,
so the named volume otherwise comes up root-owned and tmux refuses to
bind its socket. Harmless no-op on Linux. `claude-container` and `ttyd`
both `depends_on` the init service with `condition:
service_completed_successfully`, so cold-start ordering is guaranteed.

### Theme + font

The default xterm.js theme is Solarized-dark (Ethan Schoonover's public-domain
palette). The default font is `Menlo` with fallback to `monospace`. Override
either by editing the `-t theme=…` / `-t fontFamily=…` flags in the
`ttyd.command` list in `docker-compose.yml`, or by forking
`ttyd/Dockerfile` to bundle a custom font into the image.

### Autodark (page chrome matches system color-scheme)

The ttyd image bundles a build-time-patched `index.html` that adds a
`prefers-color-scheme` media-query CSS block so the page background
around the xterm.js terminal flips between Solarized base03 (dark) and
base3 (light) to match the operator's system color-scheme. Without this,
macOS Safari / Chrome in dark mode would render a white frame around the
dark terminal.

A small `<script>` block in the same patch flips the xterm.js runtime
theme to match the system color-scheme on page load, and reapplies it on
a 2-second `setInterval` to defend against ttyd's post-connect
`SET_PREFERENCES` WebSocket message that otherwise overwrites the
client-side theme. Full details in
[`ttyd/inject-autodark.py`](ttyd/inject-autodark.py).

If you don't want the autodark behavior, drop the `-I /usr/local/share/ttyd/index.html`
line from `ttyd.command` in `docker-compose.yml`. ttyd will then serve
its upstream bundled HTML unchanged.

### Security note

The published port `7681` is unauthenticated by design for local-only
development. Do **NOT** expose this port publicly without adding an
authentication layer in front (oauth2-proxy, nginx basic-auth, Cloudflare
Access, etc.). ttyd's `--writable` flag means anyone reaching the port has
full shell access to the in-container tmux session — which, by extension,
has read access to the bind-mounted `~/.claude` and `~/repos`.

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

### macOS — host UID 501 vs container UID 1000

macOS user accounts default to **uid 501**, not 1000 — and the container images bake the in-container user at uid/gid `1000:1000` (with `queue-minisite` / `eichi-search` pinning `user: "1000:1000"` in this compose file). The two don't match, and yet the stack works on macOS without any manual fixup. Why:

- **Docker Desktop (macOS / Windows)** runs the engine inside a hidden VM and routes bind mounts through a userland file-sharing layer (gRPC-FUSE / VirtioFS). That layer transparently remaps file ownership so the container sees its expected uid (1000) regardless of the host file's actual owner on the Mac filesystem. Reads + writes round-trip without permission errors. This is purely a Docker Desktop convenience and does NOT apply to Linux.
- **Linux dev boxes** (native Docker Engine, no Docker Desktop) run the engine directly against the host kernel — bind mounts pass through unchanged, so a uid-1000 container process writing to a host directory owned by uid 1500 will produce files literally owned by uid 1000 on the host, and reading host-owned files may EACCES depending on mode bits.

If you're on a Linux box with a non-1000 host UID, you have three options, in order of decreasing effort:

1. **Run as a uid-1000 user.** Easiest if you're setting up a dedicated dev account anyway — `useradd -u 1000 ...` (or repurpose the existing user) and everything just works.
2. **Override `user:` on each service.** Replace the `user: "1000:1000"` lines in `docker-compose.yml` with `user: "$(id -u):$(id -g)"` (or expand the literal numbers) and `chown -R` the bind-mounted host directories to that same uid/gid before launching. The container's named user (`hndrewaall`) is still uid 1000 inside the image; the override only changes which uid the process runs as.
3. **Rebuild the images with matching uid/gid.** Pass `--build-arg HOSTUID=$(id -u) --build-arg HOSTGID=$(id -g)` through and extend the Dockerfile's `useradd` line. Heaviest path, only worth it for long-lived deployments.

The `claude-container` service does NOT pin a `user:` directive — it inherits from the Dockerfile's `USER hndrewaall` (uid 1000) directly. On Linux with a non-1000 host UID, bind-mounted `~/.claude` / `~/repos` will look root-owned to the container; same options apply.

### Skipping services

`claude-container`, `queue-minisite`, and `eichi-search` are independent — comment any one out in `docker-compose.yml` and the rest still work. For example, `docker compose up queue-minisite eichi-search` skips the heavy Rust + Node build of the claude-container image. The `ttyd` service has a hard dependency on `claude-container` (nothing to attach to otherwise); skip both together or neither.

### No upstream auth gate

`queue-minisite` and `eichi-search` are designed to sit BEHIND an authentication proxy (oauth2-proxy, nginx `auth_request`, etc.). The included compose binds them directly to `localhost:8000` / `localhost:8001` with no gate — fine for local single-user dev, NOT fine for exposure on a public IP. Don't `-p 0.0.0.0:8000:8000` this without an auth layer in front.

## Tear down

```sh
docker compose down              # stop + remove containers (volumes survive)
docker compose down -v           # also nuke the claude-container-versions volume
```

The bind-mounted host state under `~/.claude`, `~/.config/session`, `~/.local/share/eichi`, etc. is untouched by `down` — only named volumes go.
