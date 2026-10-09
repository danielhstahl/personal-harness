## Personal Harness

This is a repository with my personal configurations and docker containers for agentic coding.

### Global configuration

[models.json](./models.json) should go in your .pi/agents directory.  

If you are connecting to any engine that does not parse reasoning into seperate message types, use something like this in your `campat` object: 

```json
"thinkingFormat": "qwen-chat-template",
"thinkingTokenBudgetField": "thinking_budget_tokens"
```

The backend api can be set to openai or anthropic:

```json
"api": "openai-completions", 
```
or
```json
"api":  "anthropic-messages",
```

Note that this harness does not work well if the following two conditions are both met:
* `litellm` is used to proxy
* `litellm`'s backend uses `hosted_vllm/<model>`
There appears to be a bug that causes tool calls to not consistently be parsed, resulting in premature halting of `pi.dev`.  

### pi-beads
[pi-beads](./pi-beads) contains my currently preferred harness.  This uses [pi.dev](https://pi.dev) with [beads](https://beads.gascity.com/) to create an organized, long-running, and resilient agent.  I use local LLMs so all agent calls need to be sequential for performance, and this combination fits the bill perfectly.  I have a custom orchestrator that follows this state machine:

* State 0: Harness starts.  Go to State 1.
* State 1: Start new session (no persisted context).  Checks for any `beads` in "ready" state.  If yes, go to State 4.  Else, State 2. 
* State 2: Wait for input from human.  On input, go to State 3.
* State 3: Take human input and translate this into one or more `beads`.  Use a `split` tool.  Go to State 1.
* State 4: Start new session (no persisted context).  Select top "ready" `bead` and "claim" it.  Work `bead`.  Go to State 5.
* State 5: "Close" `bead`.  Commit work.  Use `bd remember` to store anything that future agents may need.  Go to State 1.

#### Previous approach
The Agent can create new tickets and pick them up later to work.  If the Agent fails for any reason, it will pick right back up from the last state of the beads Kanban board.  Instructions for pi-beads are in [AGENTS.md](./pi-beads/AGENTS.md) which should go in your .pi/agents directory.

My typical workflow is to create an epic with high level instructions, and then ask `pi` to split into separate tickets.  [AGENTS.md](./pi-beads/AGENTS.md) tells `pi` to only create new issues as `low` and `oneshot`, which prevents it from trying to "hand off" to a human or another agent persona (which is clunky and non-automated).

### pi-teams
[pi-teams](./pi-teams) uses [pi.dev](https://pi.dev) with the `@giladbarnea/pi-simple-team` extension.  While this "works", it really isn't suitable for local LLMs (unless you have multiple servers each running an LLM!) since prompt caching breaks and token throughput slows to a crawl.


##todos
* When I type it goes off the side of the screen.  Fix input (and possibly tool output as well, tool commands go off the screen) [fixed]
* Note there is no terminal during beeds (which is "good" but I would like a normal bash eventually) [FIXED]
* Pi entry shouldn't exist when Pi is going (much like beads) [FIXED]
* src/session/view set_status seems unclear and doesn't have consistent behavior across terminal/input types (beads, pi, bash) [FIXED]
* Where should I put a notification trigger for a bead completing?  I want it to be async and decoupled but I can't find the "trigger" in the code... [FIXED]
* Once agent is "done done" it invokes `let _ = self.cmd_tx.try_send(UiCommand::BeadsNext);`.  I can see in the logs that it does so, but it is never received/acted on in the BeadLoop (listen_input).  So the loop stalls after completing a ticket. [FIXED]
* I think that the shell (or not) gets rendered on next tick,  not immediately on tab.  The "state" gets registers on tab, but the rerender doesn't immediate occur (unlike typing which i believe is immediate) [correct, but intentional]
* Dropped bytes is useless, use context input/output.  Add token/context window [Done]
* During output part will be cutoff by top until it is "committed" to the scrollback [fixed, with status at bottom. still "jumps" slightly on scrollback]
* Arrow keys don't navigate text [done, + shift enter]
* put pi install npm:@piotr-oles/pi-bash-timeout in docker
* Resize still can mess up view (especially when reducing size)
* Consider adding a message for compaction (otherwise it might just sit there compacting with no notification to the user)
* During agentic workflows sometimes nothing will look like its happening, but clearly something is (it starts moving again with a quick tool command going green).
* full screen app?





This repo is half-done implementing a complex agent harness that has three terminal states.  The first is a beeds state that runs a loop creating tickets and then working them.  The second is a normal pi session.  The third is a plain bash.  Can you help complete this implementation?
