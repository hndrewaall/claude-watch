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

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        tmux \
        bash \
        ncurses-term \
        tini \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=upstream /usr/bin/ttyd /usr/bin/ttyd

ENTRYPOINT ["/usr/bin/tini", "--"]
