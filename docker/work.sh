#!/bin/bash
SESSION="dev-$(basename "$PWD" | tr '.:' '__')"
DIR="$PWD"

tmux has-session -t "$SESSION" 2>/dev/null && exec tmux attach -t "$SESSION"


tmux new-session -d -s "$SESSION" -c "$DIR"
#tmux set-option -t "$SESSION" destroy-unattached on

# Top-left: fathom
FATHOM=$(tmux display-message -p -t "$SESSION" '#{pane_id}')
tmux send-keys -t "$FATHOM" "fathom index && fathom --theme ansi" C-m

# Right: harness (large)
HARNESS=$(tmux split-window -h -p 65 -t "$FATHOM" -c "$DIR" -P -F '#{pane_id}')
tmux send-keys -t "$HARNESS" "harness.sh" C-m

# Bottom-left: plain terminal (shortest)
TERM_PANE=$(tmux split-window -v -p 25 -t "$FATHOM" -c "$DIR" -P -F '#{pane_id}')

# Focus the plain terminal
tmux select-pane -t "$TERM_PANE"

# Attach, then enable destroy-unattached once a client is connected
exec tmux attach -t "$SESSION" \; set-option destroy-unattached on
