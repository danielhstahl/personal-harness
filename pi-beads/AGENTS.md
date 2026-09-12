## Task tracking (bd + pi-workgraph)

This project uses beads (`bd`) for issue tracking, with pi-workgraph layered on
top for coordinated claiming and lifecycle management.

- **Creating work**: use plain `bd create "title" -p <priority> -t <type>` and
  `bd dep add` directly via bash. This is safe — creation never touches lease
  state, so it doesn't need a typed tool.
- **Claiming, approving, closing, checking status**: use the `workgraph_*`
  tools (`workgraph_ready`, `workgraph_claim`, `workgraph_approve`,
  `workgraph_close`, `workgraph_split`, `workgraph_status`,
  `workgraph_release`, `workgraph_heartbeat`) — never raw `bd update --claim`
  or `--assignee`, which can corrupt lease fencing.
- **New issues start in `draft`** and are invisible to the coordinator until
  approved via `workgraph_approve` with acceptance criteria, a workflow class
  (`oneshot`/`reviewed`/`planned`), and a risk tier.  IMPORTANT: all issues
  should be created with a `low` risk tier.  All issues should be created with
  class `oneshot`.
- **Git commit work** once an issue is complete.
- Run `bd prime` for a refresher on other bd commands and stored project
  memory.
