# ttyd web-console for the claude-container tmux session.
#
# Built FROM debian:bookworm-slim so the in-container tmux client
# version matches the tmux server running in the claude-container
# sibling service exactly. Both use the bookworm `tmux` apt package
# (currently 3.3a). This matters because the tmux wire protocol is
# strict about client/server version match — attaching a tmux 3.6
# client (Alpine 3.23's package) to a tmux 3.3a server (Debian
# bookworm's package) fails with the misleading error
# "open terminal failed: not a terminal" instead of a version-skew
# message. See tmux issue #4356 for the upstream confirmation.
#
# The upstream image only ships Alpine + Ubuntu tags, so we copy
# the prebuilt static ttyd binary out of tsl0922/ttyd:1.7.7
# (Ubuntu-based; the binary is statically linked and works fine on
# bookworm) and install tmux + tini from bookworm apt.
#
# Pin a concrete upstream tag for reproducibility.
FROM tsl0922/ttyd:1.7.7 AS upstream

# -------------------------------------------------------------------
# Stage: extract upstream HTML and inject autodark CSS + theme-swap JS.
#
# ttyd 1.7.7 ships a single self-contained index.html (~730 KB, all
# CSS + JS inlined) embedded in the binary. There's no static-asset
# directory to drop a CSS file into — `-I/--index` is the only hook,
# and it replaces the served HTML entirely. So at build time we:
#   1. Boot ttyd briefly inside this stage on 127.0.0.1.
#   2. Fetch the bundled HTML via wget.
#   3. Run our inject script to splice <style>/<script> before </head>.
#   4. Carry the patched HTML into the runtime stage.
#
# The patched HTML adds:
#   - A @media (prefers-color-scheme) <style> block painting body /
#     html background Solarized base03 (dark) or base3 (light) so the
#     page chrome around xterm.js matches the system color-scheme.
#   - A <script> that flips xterm.js's runtime theme to Solarized
#     dark/light matching prefers-color-scheme, with a setInterval
#     reapply to defend against ttyd's post-connect SET_PREFERENCES
#     overwrite (full race details in inject-autodark.py).
# -------------------------------------------------------------------
FROM debian:bookworm-slim AS html-builder
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 \
        wget \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=upstream /usr/bin/ttyd /usr/bin/ttyd
COPY inject-autodark.py /tmp/inject-autodark.py
# Run ttyd in the background on loopback, fetch its self-served HTML,
# inject, then kill the bootstrap ttyd. `echo hi` is a placeholder
# command — ttyd needs a child program but exits on first close, which
# is fine because we only need the HTTP listener to come up briefly.
RUN set -eux; \
    ttyd -p 17681 -i 127.0.0.1 echo hi & \
    TTYD_PID=$!; \
    for i in 1 2 3 4 5 6 7 8 9 10; do \
        wget -q -O /tmp/index.upstream.html http://127.0.0.1:17681/ && break || sleep 0.5; \
    done; \
    test -s /tmp/index.upstream.html; \
    kill $TTYD_PID 2>/dev/null || true; \
    wait $TTYD_PID 2>/dev/null || true; \
    python3 /tmp/inject-autodark.py /tmp/index.upstream.html /tmp/index.patched.html; \
    grep -q 'prefers-color-scheme' /tmp/index.patched.html; \
    grep -q 'autodark-injected' /tmp/index.patched.html

# -------------------------------------------------------------------
# Runtime stage.
# -------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        tmux \
        bash \
        ncurses-term \
        tini \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=upstream /usr/bin/ttyd /usr/bin/ttyd
COPY --from=html-builder /tmp/index.patched.html /usr/local/share/ttyd/index.html

ENTRYPOINT ["/usr/bin/tini", "--"]
