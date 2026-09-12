## Personal Harness

This is a repository with my personal configurations and docker containers for agentic coding.

[models.json](./models.json) should go in your .pi/agents directory.

### pi-beads
[pi-beads](./pi-beads) contains my currently preferred harness.  This uses [pi.dev](https://pi.dev) with [beads](https://beads.gascity.com/) to create an organized, long-running, and resilient agent.  I use local LLMs so all agent calls need to be sequential for performance, and this combination fits the bill perfectly.  The Agent can create new tickets and pick them up later to work.  If the Agent fails for any reason, it will pick right back up from the last state of the beads Kanban board.  Instructions for pi-beads are in [AGENTS.md](./pi-beads/AGENTS.md) which should go in your .pi/agents directory.

My typical workflow is to create an epic with high level instructions, and then ask `pi` to split into separate tickets.  [AGENTS.md](./pi-beads/AGENTS.md) tells `pi` to only create new issues as `low` and `oneshot`, which prevents it from trying to "hand off" to a human or another agent persona (which is clunky and non-automated).

### pi-teams
[pi-teams](./pi-teams) uses [pi.dev](https://pi.dev) with the `@giladbarnea/pi-simple-team` extension.  While this "works", it really isn't suitable for local LLMs (unless you have multiple servers each running an LLM!) since prompt caching breaks and token throughput slows to a crawl.
