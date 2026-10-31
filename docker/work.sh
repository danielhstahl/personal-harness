#!/bin/bash
SESSION="dev"
DIR="$PWD"

tmux has-session -t "$SESSION" 2>/dev/null && exec tmux attach -t "$SESSION"

# Top-left: fathom
tmux new-session -d -s "$SESSION" -c "$DIR"
FATHOM=$(tmux display-message -p -t "$SESSION" '#{pane_id}')
tmux send-keys -t "$FATHOM" "fathom index && fathom --theme ansi" C-m

# Right: harness (large)
HARNESS=$(tmux split-window -h -p 65 -t "$FATHOM" -c "$DIR" -P -F '#{pane_id}')
tmux send-keys -t "$HARNESS" "harness.sh" C-m

# Bottom-left: plain terminal (shortest)
TERM_PANE=$(tmux split-window -v -p 25 -t "$FATHOM" -c "$DIR" -P -F '#{pane_id}')

# Focus the plain terminal
tmux select-pane -t "$TERM_PANE"

tmux attach -t "$SESSION"
