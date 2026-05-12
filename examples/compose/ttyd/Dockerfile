# ttyd web-console for the claude-container tmux session.
#
# Minimal layer on top of upstream tsl0922/ttyd:alpine — adds tmux
# (and bash for shell parity) so the in-container tmux socket shared
# from the sibling claude-container service is attachable.
#
# Pinning to :alpine (rolling) is fine for an example. Pin a concrete
# tag (e.g. `1.7.7-alpine`) if you need reproducible builds.
FROM tsl0922/ttyd:alpine

RUN apk add --no-cache tmux bash ncurses-terminfo
