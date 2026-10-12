//! The two standing prompts the beads loop runs on: the planner's, and the
//! worker's.
//!
//! The worker's is a *function* of the typed-tool surface rather than a constant,
//! and that is the whole point of this module's existence: the prompt has to name
//! the `bd` verbs the child was actually given (`bd_close`, or `bd close <id>`),
//! because a worker told to run a command it has no tool for and no shell to run
//! it in settles quietly with the ticket still open. One value
//! [`Surface`](crate::services::bd_tools::Surface) decides the argv and the
//! wording, so the two cannot disagree — see
//! [`services::bd_tools`](crate::services::bd_tools) for the other half.

use crate::services::bd_tools::Surface;

pub const PLANNER: &str = r#"
You are a technical software engineering manager.  You take the requirements, translate the requirements into technical details, and create one or more tickets representing the work.

You do not do any code development yourself.

The ticket management system is beads.  Commands:

bd create [title] [flags]
  Common flags:
  -p, --priority string         Priority (0-4 or P0-P4, 0=highest) (default "2")
  -t, --type string             Issue type (bug|feature|task|epic|chore|decision); custom types require types.custom config; aliases: enhancement/feat→feature, dec/adr→decision (default "task")
  --parent string               Parent issue ID for hierarchical child (e.g., 'bd-a3f8e9')
bd ready              # Find available work
bd show <id>          # View issue details

Examples:

bd create "Set up database" -p 1 -t task
bd create "Create API" -p 2 -t feature
bd create "Add authentication" -p 2 -t feature

These are the instructions that you need to break down into tickets:
"#;

/// The worker's standing instructions, with the `bd` verbs spelled the way this
/// pass was actually given them.
///
/// One template with three holes rather than two prompts, because everything
/// outside the holes — the claim is already taken, exactly one ticket per pass,
/// the board rather than the model's word is the proof, commit the work — is the
/// part that must not differ between a worker with typed tools and one without.
/// The holes are the `bd` block, and the two closing bullets that name a `bd` verb.
const WORKER: &str = r#"
You are a technical software engineer.  You work the ticket you have been given and finish it.

{{BD}}

Your ticket has ALREADY been claimed for you by the harness, and it is named below.
There is no work-discovery step here and you must not go looking for other work:
the loop above you reports, guards and pays for exactly one ticket, and a pass
spent on a different one is invisible to it.

When the work is complete, do the following:
* Ensure the ticket you were assigned is closed with {{CLOSE}}. The harness checks the board, not your intentions: a ticket you settled without closing stops the loop rather than being retried.
* Ensure any relevant notes are saved with {{REMEMBER}}
* Use `git` to commit your work with a relevant commit message

"#;

/// The `bd` block: the verbs, in the shape the worker can actually use.
///
/// Two shapes, because there are two ways to hand a worker the board. With the
/// typed surface the verbs are *tools* and are written as tools — `bd_show({id})` —
/// because that is what the child can call; the CLI form is something it would have
/// to reach through bash, which is the thing the typed surface exists to make
/// unnecessary. Without it, this is the shell cheat sheet, byte for byte the one
/// this loop has always sent.
///
/// Only the verbs actually in `--tools` are listed. A prompt that advertises a tool
/// the child does not have is worse than one that says nothing, and `bd_ready` /
/// `bd_claim` in particular stay unmentioned unless an operator puts them in the
/// list: this loop claims the ticket it pays for and picks the work itself, which is
/// what the paragraph after this block tells the worker not to second-guess.
fn bd_block(surface: &Surface) -> String {
    if surface.is_off() {
        return r#"The ticket management system is beads.  Commands:

bd show <id>            # View issue details
bd close <id>           # Complete work
bd remember "<insight>" [flags]
  Relevant flag:
  --key string   Explicit key for the memory (auto-generated from content if not set). If a memory with this key already exists, it will be updated in place

Use `bd remember` to save any relevant notes for your future self."#
            .to_string();
    }
    // `(tool, what it is)`, in the extension's own registration order, and only for
    // the verbs this pass was given.
    let rows: Vec<(&'static str, &'static str)> = [
        (
            "bd_ready({limit?})",
            "ready",
            "Tickets the loop can pick up right now. Read-only.",
        ),
        (
            "bd_show({id})",
            "show",
            "One ticket in full: description, notes, children, status. Read-only.",
        ),
        (
            "bd_claim({id})",
            "claim",
            "Take a ticket so no other worker can. Fails when another worker holds the lease.",
        ),
        (
            "bd_close({id, reason?})",
            "close",
            "Close a ticket as done.",
        ),
        (
            "bd_remember({text, key?})",
            "remember",
            "Save a durable note for a later session; a matching `key` updates that note in place.",
        ),
    ]
    .into_iter()
    .filter(|(_, verb, _)| surface.has(verb))
    .map(|(tool, _, what)| (tool, what))
    .collect();
    // Column-aligned, because this block is read by a model that then has to pick one
    // and a ragged list reads like one long sentence.
    let width = rows.iter().map(|(tool, _)| tool.len()).max().unwrap_or(0);
    let mut out = String::from(
        "The ticket management system is beads.  The beads verbs are tools you have:\n\n",
    );
    for (tool, what) in &rows {
        out.push_str(&format!("{tool:<width$} # {what}\n", width = width));
    }
    out.push_str(
        "\nEach one runs `bd` and answers with `bd`'s own words, refusals included: a refusal\n",
    );
    out.push_str(
        "is information about the board, not a crash to work around. Do not shell out to `bd`\n",
    );
    out.push_str("for something one of these tools already does.");
    if surface.has("remember") {
        out.push_str("\n\nUse `bd_remember` to save any relevant notes for your future self.");
    }
    out
}

/// How to close the ticket: the tool if the pass has the tool, the CLI otherwise.
fn close_instruction(surface: &Surface) -> &'static str {
    if surface.has("close") {
        "`bd_close`"
    } else {
        "`bd close <id>`"
    }
}

/// How to save a note, same rule.
fn remember_instruction(surface: &Surface) -> &'static str {
    if surface.has("remember") {
        "`bd_remember`"
    } else {
        "`bd remember`"
    }
}

pub fn worker(surface: &Surface) -> String {
    WORKER
        .replace("{{BD}}", &bd_block(surface))
        .replace("{{CLOSE}}", close_instruction(surface))
        .replace("{{REMEMBER}}", remember_instruction(surface))
}

pub fn generate_prompt(static_prompt: &str, instructions: &str) -> String {
    // 1. Pre-allocate capacity to avoid reallocations
    let mut result = String::with_capacity(static_prompt.len() + instructions.len());
    // 2. Append efficiently
    result.push_str(static_prompt);
    result.push_str(instructions);
    result
}
