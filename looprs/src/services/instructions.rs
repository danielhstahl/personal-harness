//! Per-project, per-role additional instructions for the beads loop (workspace-cw4).
//!
//! [`AGENTS.md`](https://pi.dev) is the usual way to hand an agent project rules, and
//! it is the wrong tool here for two reasons. It is read by the engine, so it reaches
//! *every* session alike: the planner's rules ("never write code", "never leave an
//! epic childless") land on the worker, and the worker's ("work only the ticket you
//! were given") land on the planner. And it is one file for one repo, so the person
//! running the loop cannot say something different to the two roles, and cannot say
//! anything at all without editing the project the loop is pointed at.
//!
//! So this module resolves the missing layer: **text added to one role's prompt,
//! taken from configuration rather than hardcoded into the binary**, and layered
//! over the standing text by
//! [`compose`](crate::services::prompts::compose).
//!
//! ## Where the text comes from
//!
//! Three sources per role, concatenated in this order — project first, then the
//! explicit file, then the inline text — because the earlier a layer appears the
//! more it reads like the project's own standing rules, and the later it appears the
//! more it reads like what was asked for today:
//!
//! 1. `<dir>/<role>.md`, where `<dir>` is `.looprs` under the working directory
//!    unless `LOOPRS_INSTRUCTIONS_DIR` says otherwise, and `<role>` is `planner` or
//!    `worker`. This is the per-project home; commit it, and the loop carries the
//!    project's rules without anyone hardcoding them.
//! 2. the file named by `LOOPRS_PLANNER_FILE` / `LOOPRS_WORKER_FILE`, for text that
//!    lives outside the project (a shared file across several repos, a file a
//!    wrapper script generates).
//! 3. the text in `LOOPRS_PLANNER_INSTRUCTIONS` / `LOOPRS_WORKER_INSTRUCTIONS`, for
//!    one-offs that do not deserve a file.
//!
//! A source that is absent is not an error; a source that was **named** and could not
//! be read is a warning, because the silently-ignored knob is the knob that gets
//! turned again ([configuration reference](../../docs/guide/configuration.md)).
//!
//! ## The rule this obeys
//!
//! Everything here is resolved **once, at startup, from arguments** —
//! [`resolve`] reads no environment and takes the directory as a parameter, and
//! only [`RoleInstructions::from_env`] (called by `main`, once) touches either.
//! That is the crate's no-environment rule, and its payoff is that every branch
//! below is reachable from a unit test without mutating a process
//! ([contributor guide](../../docs/guide/contributing.md)).

use std::path::{Path, PathBuf};

use crate::services::prompts::{Role, RoleNotes};

/// The default directory role files are looked for in, relative to the working
/// directory. A dot-directory, because it is configuration rather than content.
pub const DIR_NAME: &str = ".looprs";

/// The ceiling on one role's instructions, in bytes.
///
/// These strings go into the prompt of *every* pass, so an unbounded read is a
/// file of any size billed on any size run — a `LOOPRS_INSTRUCTIONS_DIR` that
/// points at a log directory, or a `LOOPRS_WORKER_FILE` that someone pointed at a
/// whole concatenated manual, would quietly multiply the cost of every worker.
/// Past the ceiling the text is cut at a character boundary and the cut is
/// reported, which is the same trade the log budget makes: bounded, and loud
/// about the binding.
pub const MAX_BYTES: usize = 64 * 1024;

/// Both roles' additional instructions, resolved once at startup.
///
/// `Default` is **empty on purpose**: `SessionConfig::default()` is what several
/// hundred tests build, and a default that read the filesystem would make all of
/// them depend on what happens to be in the crate directory. The real value is
/// built by `main`, in the one place this crate is allowed to look at the
/// environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleInstructions {
    planner: Option<RoleNotes>,
    worker: Option<RoleNotes>,
    /// What went wrong on the way here: a named file that could not be read, a
    /// file that was truncated. Empty whenever everything read cleanly.
    warnings: Vec<String>,
}

impl RoleInstructions {
    /// Resolve from the process environment and the current directory.
    ///
    /// `main` calls this once and puts the result in [`SessionConfig`](crate::session::SessionConfig),
    /// so a run's instructions cannot change mid-loop — and changing them means
    /// restarting, like every other knob here. Anything that went wrong is said
    /// out loud here, at startup, where a human is reading the log.
    pub fn from_env() -> Self {
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let resolved = Self::resolve(&dir, &|key| std::env::var(key).ok());
        for warning in resolved.warnings() {
            tracing::warn!("role instructions: {warning}");
        }
        resolved
    }

    /// The pure half: `dir` is the project to look in and `look` answers every
    /// environment question, so a test can hand it a map instead of a process.
    pub fn resolve(dir: &Path, look: &dyn Fn(&str) -> Option<String>) -> Self {
        let configured_dir = trimmed(look("LOOPRS_INSTRUCTIONS_DIR"));
        let role_dir = match configured_dir.as_deref() {
            // An explicit directory is taken as given, relative or not: it is how
            // you point a loop at a shared set of rules, and rewriting it would
            // undo that.
            Some(explicit) => PathBuf::from(explicit),
            None => dir.join(DIR_NAME),
        };

        let mut out = Self::default();
        for role in [Role::Planner, Role::Worker] {
            let (file_var, inline_var) = env_names(role);
            let mut layers: Vec<Layer> = Vec::new();

            // 1. the project's own file for this role.
            let project_file = role_dir.join(role.file_name());
            layers.extend(read(
                &project_file,
                &project_file.display().to_string(),
                false,
                &mut out.warnings,
            ));

            // 2. a file named for this role, wherever it lives.
            if let Some(path) = trimmed(look(file_var)) {
                let named = PathBuf::from(&path);
                layers.extend(read(
                    &named,
                    &format!("{file_var}={path}"),
                    true,
                    &mut out.warnings,
                ));
            }

            // 3. inline text, which needs no file to be worth having.
            if let Some(text) = trimmed(look(inline_var))
                && !text.trim().is_empty()
            {
                let (text, warning) = cap(text.as_str(), inline_var);
                if let Some(warning) = warning {
                    out.warnings.push(warning);
                }
                layers.push(Layer {
                    text,
                    source: inline_var.to_string(),
                });
            }

            let notes = Layer::merge(layers);
            match role {
                Role::Planner => out.planner = notes,
                Role::Worker => out.worker = notes,
            }
        }
        out
    }

    /// The planner's additional instructions, or `None` when there are none.
    pub fn planner(&self) -> Option<&RoleNotes> {
        self.planner.as_ref()
    }

    pub fn worker(&self) -> Option<&RoleNotes> {
        self.worker.as_ref()
    }

    /// Does either role have anything to add? Used for the startup line, which
    /// has to say "nothing" as clearly as it says "here".
    pub fn is_empty(&self) -> bool {
        self.planner.is_none() && self.worker.is_none()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// One line naming where each role's instructions came from — the resolved
    /// state, which is what a person greps the log for after a run that did not
    /// behave. "none" is spelled out rather than left blank, because the question
    /// this line gets grepped for is "did my file get read?".
    pub fn describe(&self) -> String {
        if self.is_empty() {
            return "none for either role (add planner.md and worker.md to the project's \
                    instructions directory, or use the per-role file and inline variables)"
                .to_string();
        }
        format!(
            "planner: {}, worker: {}",
            describe_one(self.planner()),
            describe_one(self.worker())
        )
    }
}

/// The env var names one role answers to: `(file, inline)`.
fn env_names(role: Role) -> (&'static str, &'static str) {
    match role {
        Role::Planner => ("LOOPRS_PLANNER_FILE", "LOOPRS_PLANNER_INSTRUCTIONS"),
        Role::Worker => ("LOOPRS_WORKER_FILE", "LOOPRS_WORKER_INSTRUCTIONS"),
    }
}

fn describe_one(notes: Option<&RoleNotes>) -> String {
    match notes {
        None => "none".to_string(),
        Some(notes) => notes.sources().join(" + "),
    }
}

/// One layer of one role's instructions, already read and already labelled.
struct Layer {
    text: String,
    source: String,
}

impl Layer {
    /// Concatenate the layers that survived, and keep their labels in the same
    /// order — the prompt quotes the labels, so a mismatch between the two lists
    /// would attribute a rule to the wrong file.
    fn merge(layers: Vec<Layer>) -> Option<RoleNotes> {
        let layers: Vec<Layer> = layers
            .into_iter()
            .filter(|l| !l.text.trim().is_empty())
            .collect();
        if layers.is_empty() {
            return None;
        }
        let text = layers
            .iter()
            .map(|l| l.text.trim())
            .collect::<Vec<_>>()
            .join("\n\n");
        let sources = layers.into_iter().map(|l| l.source).collect();
        Some(RoleNotes::new(text, sources))
    }
}

/// Read one file into a layer, or say why it did not.
///
/// `label` is what the warning calls it; `required` separates "I was told to read
/// this and it is not there" from "I looked for this and it was not there", which
/// are different facts and only one of them is worth a warning.
fn read(path: &Path, label: &str, required: bool, warnings: &mut Vec<String>) -> Option<Layer> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            if required || e.kind() != std::io::ErrorKind::NotFound {
                warnings.push(format!(
                    "{label} could not be read ({e}); its instructions were not added"
                ));
            }
            return None;
        }
    };
    let (text, warning) = match String::from_utf8(bytes) {
        Ok(text) => cap(&text, label),
        Err(e) => {
            warnings.push(format!(
                "{label} is not valid UTF-8 ({}); its instructions were not added",
                e.utf8_error()
            ));
            return None;
        }
    };
    if let Some(warning) = warning {
        warnings.push(warning);
    }
    if text.trim().is_empty() {
        return None;
    }
    Some(Layer {
        text,
        source: label.to_string(),
    })
}

/// The byte ceiling, applied. Returns the text and a warning when the cut was made.
fn cap(text: &str, label: &str) -> (String, Option<String>) {
    if text.len() <= MAX_BYTES {
        return (text.to_string(), None);
    }
    // A boundary, or the cut splits a character and the prompt gains a replacement
    // glyph — which is a small ugliness that would also be in *every* pass.
    let mut end = MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (
        text[..end].to_string(),
        Some(format!(
            "{label} is {} bytes; only the first {MAX_BYTES} were used as instructions",
            text.len()
        )),
    )
}

/// An environment value, with surrounding space gone and an empty one read as
/// unset — `LOOPRS_WORKER_FILE=` in a wrapper script means "not set", not
/// "read the directory".
fn trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A throwaway project directory, unique per test so parallel tests cannot see
    /// each other's files.
    struct Project {
        dir: PathBuf,
    }

    impl Project {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "looprs-role-{}-{}-{}",
                tag,
                std::process::id(),
                Self::count()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(DIR_NAME)).expect("temp project dir");
            Self { dir }
        }

        fn count() -> u32 {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.dir.join(DIR_NAME).join(name);
            std::fs::write(&path, contents).expect("write role file");
            path
        }

        fn write_at(&self, path: &Path, contents: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).expect("parent dir");
            std::fs::write(path, contents).expect("write file");
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn lookup(map: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> {
        |key: &str| map.get(key).cloned()
    }

    /// **The feature.** A project says one thing to the planner and another to the
    /// worker, from files in its own tree, with nothing hardcoded in the binary.
    #[test]
    fn a_project_gives_each_role_its_own_instructions() {
        let p = Project::new("split");
        p.write("planner.md", "One ticket per deliverable, never more.");
        p.write("worker.md", "Run ./scripts/check.sh before committing.");
        let map = env(&[]);

        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        assert_eq!(
            r.planner().map(|n| n.text().to_string()).as_deref(),
            Some("One ticket per deliverable, never more."),
            "{r:?}"
        );
        assert_eq!(
            r.worker().map(|n| n.text().to_string()).as_deref(),
            Some("Run ./scripts/check.sh before committing."),
            "{r:?}"
        );
        // Each source is named as the file the human will go and edit.
        assert_eq!(
            r.planner().map(|n| n.sources().to_vec()),
            Some(vec![
                p.dir
                    .join(DIR_NAME)
                    .join("planner.md")
                    .display()
                    .to_string()
            ]),
            "{r:?}"
        );
        assert!(r.warnings().is_empty(), "{:?}", r.warnings());
    }

    /// Nothing configured at all — the shape every run had before this existed. No
    /// notes, no warnings, and `SessionConfig::default()` stays empty so the test
    /// suite never depends on what is in the crate directory.
    #[test]
    fn an_empty_project_adds_nothing_and_warns_about_nothing() {
        let p = Project::new("bare");
        let map = env(&[]);
        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        assert!(r.is_empty(), "{r:?}");
        assert!(r.planner().is_none() && r.worker().is_none(), "{r:?}");
        assert!(r.warnings().is_empty(), "{:?}", r.warnings());
        assert_eq!(
            r,
            RoleInstructions::default(),
            "the default is the empty case"
        );
        assert!(
            r.describe().starts_with("none for either role"),
            "\"none\" is said, not left blank: {}",
            r.describe()
        );
    }

    /// A whitespace-only file is a file nobody has filled in yet, not a rule.
    #[test]
    fn a_blank_role_file_adds_nothing() {
        let p = Project::new("blank");
        p.write("planner.md", "   \n\n\t\n");
        let map = env(&[]);
        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        assert!(r.planner().is_none(), "{r:?}");
        assert!(r.warnings().is_empty(), "{:?}", r.warnings());
    }

    /// The knob that moves the whole family, and it is taken as given rather than
    /// rewritten: a shared directory outside the project is the use case.
    #[test]
    fn the_instructions_directory_can_be_moved() {
        let p = Project::new("dir");
        let elsewhere = p.dir.join("shared/rules");
        p.write_at(&elsewhere.join("worker.md"), "Talk in imperative voice.");
        let map = env(&[(
            "LOOPRS_INSTRUCTIONS_DIR",
            elsewhere.display().to_string().as_str(),
        )]);

        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        assert_eq!(
            r.worker().map(|n| n.text().to_string()).as_deref(),
            Some("Talk in imperative voice."),
            "{r:?}"
        );
        assert!(r.planner().is_none(), "the other role is unaffected: {r:?}");
        // And the project's own `.looprs` is no longer consulted.
        p.write("worker.md", "SHOULD NOT BE READ");
        let again = RoleInstructions::resolve(&p.dir, &lookup(&map));
        assert_eq!(
            again.worker().map(|n| n.text().to_string()).as_deref(),
            Some("Talk in imperative voice."),
            "LOOPRS_INSTRUCTIONS_DIR replaces the default rather than adding to it"
        );
    }

    /// The per-role file knob, for text that lives outside the project — and it
    /// adds to the project's own file rather than replacing it, in a fixed order
    /// with both sources named.
    #[test]
    fn a_named_file_adds_to_the_project_file_in_order() {
        let p = Project::new("file");
        p.write("worker.md", "PROJECT");
        let outside = p.dir.join("elsewhere/global-worker.md");
        p.write_at(&outside, "GLOBAL");
        let map = env(&[("LOOPRS_WORKER_FILE", outside.display().to_string().as_str())]);

        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        let notes = r.worker().expect("both layers are present");
        assert_eq!(notes.text(), "PROJECT\n\nGLOBAL", "{notes:?}");
        assert_eq!(
            notes.sources(),
            &[
                p.dir.join(DIR_NAME).join("worker.md").display().to_string(),
                format!("LOOPRS_WORKER_FILE={}", outside.display()),
            ],
            "{notes:?}"
        );
        assert!(r.planner().is_none(), "the knob is per-role: {r:?}");
    }

    /// Inline text is the last layer and the quickest way in — no file needed,
    /// which is what makes it usable from a wrapper script or a one-off run.
    #[test]
    fn inline_text_lands_last_and_names_its_variable() {
        let p = Project::new("inline");
        p.write("planner.md", "PROJECT");
        let map = env(&[(
            "LOOPRS_PLANNER_INSTRUCTIONS",
            "  Prefer epics over many loose tickets.  ",
        )]);
        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        let notes = r.planner().expect("two layers");
        assert_eq!(
            notes.text(),
            "PROJECT\n\nPrefer epics over many loose tickets."
        );
        assert_eq!(
            notes.sources()[1],
            "LOOPRS_PLANNER_INSTRUCTIONS",
            "{notes:?}"
        );
    }

    /// All three sources at once, in the documented order, for both roles.
    #[test]
    fn all_three_layers_stack_in_the_documented_order() {
        let p = Project::new("stack");
        p.write("worker.md", "ONE");
        let extra = p.dir.join("x.md");
        p.write_at(&extra, "TWO");
        let map = env(&[
            ("LOOPRS_WORKER_FILE", extra.display().to_string().as_str()),
            ("LOOPRS_WORKER_INSTRUCTIONS", "THREE"),
        ]);
        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        let notes = r.worker().expect("three layers");
        assert_eq!(notes.text(), "ONE\n\nTWO\n\nTHREE", "{notes:?}");
        assert_eq!(notes.sources().len(), 3, "{notes:?}");
    }

    /// **A file that was named and is not there is a warning, not a shrug.** The
    /// knob was turned; the loop has to say it did not take.
    #[test]
    fn a_named_file_that_cannot_be_read_is_warned_about() {
        let p = Project::new("missing");
        let gone = p.dir.join("nope.md");
        let map = env(&[("LOOPRS_PLANNER_FILE", gone.display().to_string().as_str())]);
        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        assert!(r.planner().is_none(), "{r:?}");
        assert_eq!(r.warnings().len(), 1, "{r:?}");
        assert!(
            r.warnings()[0].contains("LOOPRS_PLANNER_FILE")
                && r.warnings()[0].contains("could not be read"),
            "{:?}",
            r.warnings()
        );
    }

    /// …and the *absence* of a project file is never a warning: that is the normal
    /// case, and warning on it would be a startup line on every run of every repo
    /// that has no `.looprs`.
    #[test]
    fn an_absent_project_file_is_silent_but_an_unreadable_one_is_not() {
        let p = Project::new("unreadable");
        let map = env(&[]);
        assert!(
            RoleInstructions::resolve(&p.dir, &lookup(&map))
                .warnings()
                .is_empty(),
            "nothing to read is nothing to say"
        );
        // A directory where a role file should be: `read` fails with `IsADirectory`
        // on Linux, which is not `NotFound` and so must be reported.
        std::fs::create_dir_all(p.dir.join(DIR_NAME).join("worker.md")).expect("dir as file");
        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        assert_eq!(r.warnings().len(), 1, "{r:?}");
        assert!(r.warnings()[0].contains("worker.md"), "{:?}", r.warnings());
    }

    /// Non-UTF-8 is refused rather than laundered through replacement characters:
    /// a role file that is really a binary is a mistake, and the prompt should not
    /// silently carry it on every pass.
    #[test]
    fn a_file_that_is_not_text_is_refused_with_a_warning() {
        let p = Project::new("binary");
        let path = p.dir.join(DIR_NAME).join("planner.md");
        std::fs::write(&path, [0u8, 159, 141, 148, 0xFF]).expect("write bytes");
        let map = env(&[]);
        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        assert!(r.planner().is_none(), "{r:?}");
        assert!(
            r.warnings()
                .iter()
                .any(|w| w.contains("not valid UTF-8") && w.contains("planner.md")),
            "{:?}",
            r.warnings()
        );
    }

    /// **The ceiling is real.** These strings are billed on every pass, so a file
    /// of any size cannot be allowed to ride into every prompt: past `MAX_BYTES`
    /// the text is cut and the cut is reported.
    #[test]
    fn an_over_long_role_file_is_cut_at_the_ceiling_and_says_so() {
        let p = Project::new("huge");
        let big = "x".repeat(MAX_BYTES + 4096);
        p.write("worker.md", &big);
        let r = RoleInstructions::resolve(&p.dir, &lookup(&env(&[])));
        let notes = r.worker().expect("the first of it is still instructions");
        assert_eq!(notes.text().len(), MAX_BYTES, "{:?}", notes.text().len());
        assert_eq!(r.warnings().len(), 1, "{:?}", r.warnings());
        assert!(
            r.warnings()[0].contains(&MAX_BYTES.to_string()),
            "{:?}",
            r.warnings()
        );
    }

    /// The cut never splits a character: a replacement glyph in the prompt would be
    /// paid for on every pass, forever.
    #[test]
    fn the_cut_lands_on_a_character_boundary() {
        // One multi-byte character straddling the ceiling.
        let mut big = "a".repeat(MAX_BYTES - 1);
        big.push('é');
        let (cut, warning) = cap(&big, "test");
        assert!(cut.is_char_boundary(cut.len()), "{cut:?}");
        assert!(warning.is_some(), "the cut must still be reported");
        assert!(!cut.contains('\u{fffd}'), "{cut}");
    }

    /// **The environment read, exercised for real.** `from_env` is the only thing
    /// in this module that touches the process environment, `main` is the only
    /// thing that calls it, and every other test here goes through the pure
    /// `resolve` — which is why the suite never reads an environment variable.
    ///
    /// Ignored because it mutates what the whole process shares, the same way the
    /// board's knob test is ([`crate::services::board_poller`]); run it with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore = "mutates the process environment"]
    fn the_environment_configures_the_role_instructions_it_is_read_from() {
        let p = Project::new("fromenv");
        p.write("worker.md", "PROJECT");
        p.write("planner.md", "PLANNER PROJECT");
        let extra = p.dir.join("out.md");
        p.write_at(&extra, "EXTRA");
        let missing = p.dir.join("nowhere.md");
        let dir = p.dir.join(".looprs");

        // SAFETY: `#[ignore]`d, so this runs only when asked for, and nothing
        // else in the ignored set reads these five variables.
        unsafe {
            std::env::set_var("LOOPRS_INSTRUCTIONS_DIR", &dir);
            std::env::set_var("LOOPRS_WORKER_FILE", &extra);
            std::env::set_var("LOOPRS_WORKER_INSTRUCTIONS", "INLINE");
            std::env::set_var("LOOPRS_PLANNER_FILE", &missing);
        }
        let r = RoleInstructions::from_env();
        unsafe {
            std::env::remove_var("LOOPRS_INSTRUCTIONS_DIR");
            std::env::remove_var("LOOPRS_WORKER_FILE");
            std::env::remove_var("LOOPRS_WORKER_INSTRUCTIONS");
            std::env::remove_var("LOOPRS_PLANNER_FILE");
        }

        assert_eq!(
            r.worker().map(|n| n.text().to_string()).as_deref(),
            Some("PROJECT\n\nEXTRA\n\nINLINE"),
            "all three layers, in order: {r:?}"
        );
        assert_eq!(
            r.planner().map(|n| n.text().to_string()).as_deref(),
            Some("PLANNER PROJECT"),
            "{r:?}"
        );
        assert_eq!(
            r.warnings().len(),
            1,
            "the planner's missing file is said: {:?}",
            r.warnings()
        );
        assert!(r.describe().contains("worker:"), "{}", r.describe());
    }

    /// An empty or whitespace-only value means "not set", which is what a wrapper
    /// script that exports a variable unconditionally produces.
    #[test]
    fn a_blank_environment_value_is_unset_not_a_rule() {
        let p = Project::new("blankenv");
        let map = env(&[
            ("LOOPRS_PLANNER_FILE", "   "),
            ("LOOPRS_WORKER_INSTRUCTIONS", ""),
        ]);
        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        assert!(r.is_empty(), "{r:?}");
        assert!(
            r.warnings().is_empty(),
            "an unset knob must not warn about a file called \"   \" : {:?}",
            r.warnings()
        );
    }

    /// The startup line names what was picked up, including "none", so the log
    /// answers "did my file get read?" without a second run.
    #[test]
    fn the_description_names_every_source_that_was_read() {
        let p = Project::new("describe");
        p.write("planner.md", "A");
        let map = env(&[("LOOPRS_WORKER_INSTRUCTIONS", "B")]);
        let r = RoleInstructions::resolve(&p.dir, &lookup(&map));
        let described = r.describe();
        assert!(described.contains("planner: "), "{described}");
        assert!(described.contains("planner.md"), "{described}");
        assert!(
            described.contains("LOOPRS_WORKER_INSTRUCTIONS"),
            "{described}"
        );
        // And the role accessor agrees with the field it reads.
        assert_eq!(r.worker().map(|n| n.text()), Some("B"));
        assert!(Role::Planner.file_name().ends_with("planner.md"));
    }
}
