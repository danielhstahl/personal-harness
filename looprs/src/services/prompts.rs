//! The standing prompt text of the two beads roles, and the one function that
//! composes a prompt out of its three parts.
//!
//! A prompt is three things in a fixed order, and the order is the whole content
//! of this module:
//!
//! 1. the **standing** text of the role — what a planner *is*, what a worker *is*,
//!    which `bd` commands it may run and what it must hand back;
//! 2. the **project's additional instructions for that role** ([`RoleNotes`],
//!    resolved by [`crate::services::instructions`]) — per-project text the person
//!    running the loop owns, and which this repo deliberately does not hardcode;
//! 3. the **per-pass** text — the request a human just typed, or the ticket this
//!    pass holds a claim on.
//!
//! The ordering of 2 before 3 is not cosmetic. The planner's standing text ends
//! with "These are the instructions that you need to break down into tickets:", so
//! project instructions placed *after* it would be read as requirements to plan,
//! which is the opposite of what they are: standing rules that outlive the request.
//!
//! The split into a head and a trailer (`PLANNER_HEAD` + `PLANNER_ASK`) exists for
//! exactly that reason: the trailer has to stay glued to the request, whatever the
//! middle section turns out to be.

pub const PLANNER_HEAD: &str = r#"
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

Epics and children:

An epic (or any parent ticket) is a CONTAINER: it holds the plan, and its children hold the work.
`bd ready` lists a parent before its children — it knows about blockers, not about containment —
so a parent with nothing under it costs a pass spent reading a paragraph of intent.

* Create the epic first, then create every piece of work underneath it with `--parent <epic-id>`.
* Never leave an epic without children. If the whole epic is one ticket's worth of work, create
  one ticket instead of an epic.
* Make each child independently finishable, and say in its title what it delivers. The loop works
  children before their parent and the parent last, so a child is what actually gets built.
* Do not write a child that just says "do the epic": that is the ticket you already wrote.

Examples (a real plan, in order):

bd create "Epic: persistence" -p 1 -t epic                 # -> e.g. app-9k2
bd create "Add the schema" -p 2 -t task --parent app-9k2
bd create "Write the migration runner" -p 2 -t task --parent app-9k2

"#;

pub const PLANNER_ASK: &str =
    "These are the instructions that you need to break down into tickets:\n";

pub const WORKER: &str = r#"
You are a technical software engineer.  You work the ticket you have been given and finish it.

The ticket management system is beads.  Commands:

bd show <id>            # View issue details
bd close <id>           # Complete work
bd remember "<insight>" [flags]
  Relevant flag:
  --key string   Explicit key for the memory (auto-generated from content if not set). If a memory with this key already exists, it will be updated in place

Use `bd remember` to save any relevant notes for your future self.

Your ticket has ALREADY been claimed for you by the harness, and it is named below.
There is no work-discovery step here and you must not go looking for other work:
the loop above you reports, guards and pays for exactly one ticket, and a pass
spent on a different one is invisible to it.

When the work is complete, do the following:
* Ensure the ticket you were assigned is closed with `bd close <id>`. The harness checks the board, not your intentions: a ticket you settled without closing stops the loop rather than being retried.
* Ensure any relevant notes are saved with `bd remember`
* Use `git` to commit your work with a relevant commit message

"#;

/// Which role a prompt is written for. Two roles, because the beads loop has two
/// jobs and the instructions a person wants to add differ per job — that
/// difference is the whole reason this exists (workspace-cw4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Planner,
    Worker,
}

impl Role {
    /// The name the prompt says, in the shape the standing text uses.
    pub fn label(self) -> &'static str {
        match self {
            Role::Planner => "PLANNER",
            Role::Worker => "WORKER",
        }
    }

    /// The file name this role's project instructions go by inside the role
    /// directory (`.looprs/planner.md`, `.looprs/worker.md`).
    pub fn file_name(self) -> &'static str {
        match self {
            Role::Planner => "planner.md",
            Role::Worker => "worker.md",
        }
    }
}

/// One role's additional instructions, plus what they came from.
///
/// The sources are kept next to the text rather than dropped because the agent is
/// told them: a prompt that says which file a rule came from is one an agent can
/// go read, and one a human can go fix. This is also the only way a run can be
/// audited later — a transcript quoting "run the gate before committing" is
/// worth more when it also says that came from `.looprs/worker.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleNotes {
    text: String,
    sources: Vec<String>,
}

impl RoleNotes {
    /// Notes assembled from `sources`, in the order given. An empty `text` is
    /// *no* notes: [`compose`] will not invent a heading for nothing.
    pub fn new(text: impl Into<String>, sources: Vec<String>) -> Self {
        Self {
            text: text.into(),
            sources,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Where the text came from, in the order it was concatenated. Never empty
    /// for a note that is not empty — the two are built together in
    /// [`crate::services::instructions`].
    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// The middle section: the project's additional instructions for one role.
///
/// Written as a *supplement*, and says so. The standing text carries the harness's
/// own guarantees — a worker works the one ticket it was claimed, a planner writes
/// tickets rather than code — and those are not a person's project file's to
/// relax: a per-project note that quietly re-opened "go find work" would put the
/// runaway the guards were built for back on the board. So the section names its
/// role, names its sources, and states the precedence in one sentence.
fn notes_block(role: Role, notes: &RoleNotes) -> String {
    let sources = if notes.sources().is_empty() {
        "this run's configuration".to_string()
    } else {
        notes.sources().join(", ")
    };
    format!(
        "\n## Additional instructions for the {role} role\n\
         \n\
         These were supplied for this project by the person running this loop, from \
         {sources}. They ADD to the instructions above and replace none of them: every \
         rule above still binds. Nothing here changes which ticket you work or what \
         the harness expects back from you.\n\
         \n\
         {text}\n",
        role = role.label(),
        sources = sources,
        text = notes.text().trim_end(),
    )
}

/// Compose one role's prompt: standing text, then the project's additional
/// instructions for that role (if any), then this pass's text.
///
/// `per_pass` is the human's request for the planner and the ticket paragraph for
/// the worker — in both cases the thing that is true of *this* pass only.
///
/// The planner's trailer goes last, glued to `per_pass`, whatever the middle
/// section did (see this module's header).
pub fn compose(role: Role, notes: Option<&RoleNotes>, per_pass: &str) -> String {
    let (head, ask): (&str, &str) = match role {
        Role::Planner => (PLANNER_HEAD, PLANNER_ASK),
        Role::Worker => (WORKER, ""),
    };
    let mut prompt = String::with_capacity(head.len() + per_pass.len() + ask.len());
    prompt.push_str(head);
    if let Some(notes) = notes.filter(|n| !n.is_empty()) {
        prompt.push_str(&notes_block(role, notes));
    }
    prompt.push_str(ask);
    prompt.push_str(per_pass);
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three parts stay in their order, and the planner's trailer stays glued
    /// to the request. This is the whole point of the split: with the project
    /// section after the trailer, "These are the instructions you need to break
    /// down" would be pointing at the project's standing rules instead of at the
    /// human's request.
    #[test]
    fn the_project_section_goes_between_the_role_text_and_the_request() {
        let notes = RoleNotes::new("Always add a test.", vec![".looprs/planner.md".into()]);
        let prompt = compose(Role::Planner, Some(&notes), "make the board faster\n");
        let head = prompt
            .find("engineering manager")
            .expect("the standing text is there");
        let section = prompt
            .find("Additional instructions for the PLANNER role")
            .expect("the project section is there");
        let ask = prompt
            .find("These are the instructions that you need to break down")
            .expect("the trailer is there");
        let request = prompt
            .find("make the board faster")
            .expect("the request is there");
        assert!(
            head < section && section < ask && ask < request,
            "head={head} section={section} ask={ask} request={request}\n{prompt}"
        );
        // The trailer and the request are one breath, not separated by the section.
        assert!(
            prompt[ask..].starts_with(
                "These are the instructions that you need to break down \
                                      into tickets:\nmake the board faster"
            ),
            "{prompt}"
        );
        assert!(prompt.contains(".looprs/planner.md"), "{prompt}");
        assert!(prompt.contains("Always add a test."), "{prompt}");
    }

    /// No project instructions — the shape every run had before this existed, and
    /// the shape every test gets from `RoleNotes::default()`. The composition must
    /// be the old concatenation, byte for byte: no empty heading, no stray rule.
    #[test]
    fn with_no_project_instructions_the_prompt_is_the_old_two_parts() {
        for role in [Role::Planner, Role::Worker] {
            let per_pass = "PASS-TEXT\n";
            let none = compose(role, None, per_pass);
            let empty = compose(role, Some(&RoleNotes::default()), per_pass);
            let blank = compose(role, Some(&RoleNotes::new("   \n", vec![])), per_pass);
            assert_eq!(none, empty, "{role:?}: an empty note changed the prompt");
            assert_eq!(none, blank, "{role:?}: a blank note changed the prompt");
            assert!(!none.contains("Additional instructions"), "{none}");
            assert!(none.ends_with(per_pass), "{role:?}: {none}");
        }
        assert!(
            compose(Role::Planner, None, "X\n").ends_with("break down into tickets:\nX\n"),
            "the planner's trailer must sit directly above the request"
        );
    }

    /// The worker's section says WORKER and the planner's says PLANNER: one role's
    /// file must never read as the other's, and the name in the heading is what an
    /// agent quotes back when it mis-attributes a rule.
    #[test]
    fn each_role_is_named_in_its_own_section() {
        let notes = RoleNotes::new("RULE", vec!["src.md".into()]);
        let planner = compose(Role::Planner, Some(&notes), "REQ");
        let worker = compose(Role::Worker, Some(&notes), "TICKET");
        assert!(
            planner.contains("for the PLANNER role") && !planner.contains("for the WORKER role"),
            "{planner}"
        );
        assert!(
            worker.contains("for the WORKER role") && !worker.contains("for the PLANNER role"),
            "{worker}"
        );
        // And the worker's standing text keeps the paragraph the guards depend on.
        assert!(
            worker.contains("Your ticket has ALREADY been claimed"),
            "{worker}"
        );
    }

    /// A project note may not quietly take back the harness's own guarantees, so the
    /// section says what it is: a supplement, not a replacement.
    #[test]
    fn the_section_states_that_it_supplements_rather_than_replaces() {
        let notes = RoleNotes::new("Take whatever ticket looks easiest.", vec!["x.md".into()]);
        let worker = compose(Role::Worker, Some(&notes), "TICKET");
        assert!(worker.contains("replace none of them"), "{worker}");
        assert!(
            worker.contains("Nothing here changes which ticket you work"),
            "{worker}"
        );
        // The standing rule it must not win against is still on the page after it.
        assert!(
            worker.contains("must not go looking for other work"),
            "{worker}"
        );
    }

    /// Both sources are named, in the order the text was concatenated, so a rule in
    /// the prompt can be traced to the file it came from.
    #[test]
    fn every_source_is_named_in_the_order_it_was_concatenated() {
        let notes = RoleNotes::new(
            "one\ntwo",
            vec![
                ".looprs/worker.md".into(),
                "LOOPRS_WORKER_FILE=/tmp/x.md".into(),
            ],
        );
        let prompt = compose(Role::Worker, Some(&notes), "TICKET");
        let at_project = prompt
            .find(".looprs/worker.md")
            .expect("the project file is named");
        let at_env = prompt
            .find("LOOPRS_WORKER_FILE=/tmp/x.md")
            .expect("the env file is named");
        assert!(at_project < at_env, "{prompt}");
        assert_eq!(
            notes.sources().len(),
            2,
            "the sources are the value the prompt was built from"
        );
    }
}
