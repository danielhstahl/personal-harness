//! The typed `bd` tool surface a beads worker is given (ADR-0010 decision 2).
//!
//! The engine question is closed — `pi --mode rpc` stays a child, and what that
//! decision kept on the pi side was a schema-validated `bd` surface instead of the
//! worker shelling `bd` out of its bash tool. The extension itself is
//! [`../extensions/bd_tools.ts`](../../../extensions/bd_tools.ts); this module is
//! the Rust half of it: the knob grammar that decides *which* of its five verbs a
//! pass gets, where the source is embedded and materialised, and the argv the
//! child is spawned with.
//!
//! Three rules hold this together, and each one is a thing that would otherwise
//! drift:
//!
//! 1. **The schema lives in the extension, not here.** Nothing in this module
//!    re-describes `bd`'s arguments or invents a board status — ADR-0010 forbids a
//!    second vocabulary for the board, and ADR-0007 keeps `bd`'s own read the only
//!    honest one. What this module keeps is the *knob grammar*: the five names a
//!    `LOOPRS_BEADS_WORKER_TOOLS` value may give a `bd_*` tool, so that a typo is
//!    reported at startup instead of becoming a tool that quietly never appears.
//!    `embedded_schema_names_the_same_tools`, in this module's own tests, is what
//!    stops the two halves drifting apart.
//! 2. **One value feeds the argv *and* the prompt.** [`Surface`] is what
//!    [`SessionConfig`](crate::session::SessionConfig) carries, what
//!    [`BeadsLoop`](crate::session::beads::BeadsLoop) turns into `-e`/`--tools`,
//!    and what the worker prompt is worded from
//!    ([`prompts::worker`](crate::services::prompts::worker)). A prompt that names
//!    tools the child was not given — or hides tools it was — is the same class of
//!    bug as a loop that reports a ticket it did not claim, so there is one source
//!    and both readings go through it.
//! 3. **A pass is refused rather than run with a surface it cannot honour.** The
//!    extension has to exist on disk for `pi -e` to load it, and if it cannot be
//!    materialised the spawn fails with the knob named. Spawning anyway would buy
//!    a worker whose prompt promises verbs it does not have, at full price.
//!
//! The cost of all this is prompt, not latency, and it is stated on the extension's
//! own header and priced by
//! [`spikes/bd_tools_cost.py`](../../../spikes/bd_tools_cost.py) →
//! [`spikes/results/bd-tools-cost.log`](../../../spikes/results/bd-tools-cost.log).
//! ADR-0010 rule 7 says do not retype those numbers elsewhere; the capture owns
//! them.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The shipped extension, embedded so the tool surface travels with the binary.
///
/// Embedded rather than read out of the tree at runtime because the artifact that
/// actually runs is one copied file: `docker/Dockerfile` builds in one stage and
/// copies the *binary* into the runtime image, where there is no tree left to
/// resolve a path against. The file is one schema table, and it is read exactly
/// once per process, into a cache file `pi -e` can load.
pub const SOURCE: &str = include_str!("../../extensions/bd_tools.ts");

/// The five tool names the extension registers — and the only names a tool list
/// may give a `bd_*` tool, which is what makes a typo a startup warning instead of
/// a tool that quietly never appears.
///
/// The order is the extension's own registration order, and it is what
/// [`Surface::names`] preserves. These mirror `bd`'s own subcommands (`ready`,
/// `show`, `update --claim`, `close`, `remember`) and nothing else: this is the
/// knob's grammar, not a board vocabulary.
pub const TOOLS: [&str; 5] = ["bd_ready", "bd_show", "bd_claim", "bd_close", "bd_remember"];

/// The worker's tool list when nothing is configured: pi's four built-ins, plus
/// the three `bd` verbs a work pass actually needs.
///
/// Named in full on purpose. pi's `--tools` replaces the whole selection when the
/// list uses plain names, and its `+name`/`-name` delta form does **not** work in
/// 1.0.0 — `pi --tools +bd_show` declares *no* tools at all (checked against the
/// real CLI while this shipped, and worth re-checking before anyone "simplifies"
/// this back to a delta list). So every tool a pass needs is spelled out.
///
/// `bd_claim` and `bd_ready` are deliberately absent: the loop claims the ticket it
/// pays for and picks the work itself, which is the whole point of the worker
/// prompt's "there is no work-discovery step here". Handing those two back is a
/// knob value away for a loop that wants them.
pub const DEFAULT_WORKER_TOOLS: &str = "read,bash,edit,write,bd_show,bd_close,bd_remember";

/// The tool list values that mean "leave the worker's tools alone" — the same
/// negative spellings the board's `LOOPRS_KANBAN*` knobs accept, so one muscle
/// memory works across the knob family.
const OFF: [&str; 5] = ["off", "none", "no", "0", "false"];

/// Where the extension source comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The copy embedded in this binary, written out on first use.
    Embedded,
    /// A file the operator pointed at with `LOOPRS_BD_TOOLS_EXT`, verbatim.
    Path(PathBuf),
}

/// The resolved typed-tool surface for one run.
///
/// Plain data, resolved once at startup like every other knob, and `Debug` so a
/// failing test prints what the loop was actually configured with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Surface {
    /// The `--tools` value to pass, or `None` for "pass no tool flags at all",
    /// which is pi's own default selection (the four built-ins) and is exactly
    /// what this loop did before the typed surface shipped.
    tools: Option<String>,
    /// Where to load the extension from. `Source::Embedded` still has to be
    /// materialised — see [`Surface::extension`].
    extension: Source,
}

impl Default for Surface {
    fn default() -> Self {
        Self::from_env()
    }
}

impl Surface {
    /// Resolve from `LOOPRS_BEADS_WORKER_TOOLS` and `LOOPRS_BD_TOOLS_EXT`.
    ///
    /// Every fallback here is loud, because a silently ignored knob is a knob the
    /// operator will keep turning: each one logs a `warn` naming itself and what it
    /// did instead. The one thing it never does is half-apply — a tool list with no
    /// known `bd_*` name turns the surface off *and* takes the prompt back to the
    /// shell verbs with it, rather than leaving a worker told about tools it
    /// cannot call.
    pub fn from_env() -> Self {
        let configured = std::env::var("LOOPRS_BEADS_WORKER_TOOLS")
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if OFF.contains(&configured.to_ascii_lowercase().as_str()) {
            tracing::info!(
                "bd tools: off (LOOPRS_BEADS_WORKER_TOOLS={configured:?}); the worker shells `bd` \
                 out of bash, as it did before the typed surface"
            );
            return Self::off();
        }
        let tools = if configured.is_empty() {
            DEFAULT_WORKER_TOOLS.to_string()
        } else {
            configured.clone()
        };
        for name in Self::split(&tools) {
            if let Some(verb) = verb_of(name)
                && !TOOLS.contains(&format!("bd_{verb}").as_str())
            {
                tracing::warn!(
                    "bd tools: `{name}` is not one of the tools the extension registers ({}), so \
                     pi will declare no such tool — check `{name}` in LOOPRS_BEADS_WORKER_TOOLS",
                    TOOLS.join(", ")
                );
            }
        }

        let extension = match std::env::var("LOOPRS_BD_TOOLS_EXT")
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
            .as_str()
        {
            "" => Source::Embedded,
            raw => {
                let path = PathBuf::from(raw);
                if path.is_file() {
                    Source::Path(path)
                } else {
                    tracing::warn!(
                        "bd tools: LOOPRS_BD_TOOLS_EXT={raw} is not a file, so the extension \
                         built into this binary is used instead"
                    );
                    Source::Embedded
                }
            }
        };
        let surface = Self::new(&tools, extension);
        tracing::info!(
            "bd tools: {} (worker --tools {tools})",
            if surface.is_off() {
                "none — the worker shells `bd` out of bash".to_string()
            } else {
                surface.names().join(", ")
            }
        );
        surface
    }

    /// The bash path: pi's own default tool selection, no extension, and the
    /// worker prompt worded with `bd`'s shell commands. This is what every beads
    /// pass did before this surface existed, and it is the escape hatch for a
    /// context budget that cannot pay for the schemas.
    pub fn off() -> Self {
        Self {
            tools: None,
            extension: Source::Embedded,
        }
    }

    /// A surface from an explicit tool list and source — the constructor a test
    /// uses, since it reads no environment.
    pub fn new(tools: &str, extension: Source) -> Self {
        Self {
            tools: Some(tools.to_string()),
            extension,
        }
    }

    /// The typed tools in the tool list, in the extension's own order. Empty means
    /// the child is spawned exactly the way it was before this file existed: no
    /// `-e`, no `--tools`, `bd` through bash.
    pub fn names(&self) -> Vec<&'static str> {
        let Some(tools) = &self.tools else {
            return Vec::new();
        };
        let entries: Vec<&str> = Self::split(tools).collect();
        TOOLS
            .iter()
            .copied()
            .filter(|name| entries.contains(name))
            .collect()
    }

    /// Is any typed tool in play?
    pub fn is_off(&self) -> bool {
        self.names().is_empty()
    }

    /// Does this pass have `bd`'s `<verb>` verb as a tool? Read by the prompt
    /// wording, so `bd_close` is only promised when `--tools` names it.
    pub fn has(&self, verb: &str) -> bool {
        self.names().contains(&format!("bd_{verb}").as_str())
    }

    /// The extension file to hand `pi -e`, materialising the embedded copy if that
    /// is the source.
    pub fn extension(&self) -> Result<PathBuf, String> {
        match &self.extension {
            Source::Path(path) => {
                if path.is_file() {
                    Ok(path.clone())
                } else {
                    Err(format!(
                        "LOOPRS_BD_TOOLS_EXT points at {}, which is not a file",
                        path.display()
                    ))
                }
            }
            Source::Embedded => materialised(),
        }
    }

    /// The argv to spawn a pass with: `-e <extension> --tools <list>` — or nothing
    /// at all when no typed tool is named.
    ///
    /// The error is what a spawn turns into a failed pass, so it says what to do
    /// about it: the loop refuses the pass rather than paying for a worker whose
    /// prompt and tool list disagree.
    pub fn spawn_args(&self) -> Result<Vec<String>, String> {
        // No list at all, or a list that names none of the five: there is nothing
        // to gate and nothing to load, so the child gets what it always got.
        let Some(tools) = self.tools.clone() else {
            return Ok(Vec::new());
        };
        if self.is_off() {
            return Ok(Vec::new());
        }
        let ext = self.extension()?;
        Ok(vec![
            "-e".to_string(),
            ext.display().to_string(),
            "--tools".to_string(),
            tools,
        ])
    }

    /// Split a `--tools` value on commas, dropping blanks.
    fn split(tools: &str) -> impl Iterator<Item = &str> {
        tools.split(',').map(str::trim).filter(|s| !s.is_empty())
    }
}

/// `bd_show` → `Some("show")`, `bash` → `None`.
fn verb_of(name: &str) -> Option<&str> {
    name.strip_prefix("bd_").filter(|v| !v.is_empty())
}

/// A short, stable hash of the embedded source, used as the version tag in the
/// file name.
///
/// FNV-1a, inline, for one reason: the name only has to change when the content
/// does. A content tag is what makes it impossible for a looprs upgrade to leave a
/// stale extension behind, and it turns the write into a once-per-version cost
/// rather than a per-pass one. (Not a security decision — see [`materialised`] for
/// what is actually checked.)
fn fingerprint(source: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in source.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The cache dir: `$XDG_CACHE_HOME/looprs`, else `$HOME/.cache/looprs`, else the
/// temp dir. Created and left owner-only — see [`materialised`] for why that
/// matters more than it sounds like it does.
fn cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| Path::new(&h).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("looprs")
}

/// Write the embedded extension out, once per content version, and return the path.
///
/// Two things here are load-bearing rather than tidy. The write is atomic — a temp
/// file, then a rename — because two loops starting at once must not be able to
/// show `pi` a half-written schema. And the file is *verified* against the embedded
/// source rather than trusted for existing: an extension is code `pi` will
/// execute, and the cache dir is a place this user's other processes could have
/// written. Anything that does not match — stale, truncated, planted — is
/// overwritten from the binary, so what runs is always what this build shipped.
fn materialised() -> Result<PathBuf, String> {
    static CACHED: OnceLock<PathBuf> = OnceLock::new();
    if let Some(path) = CACHED.get() {
        return Ok(path.clone());
    }
    let dir = cache_dir();
    let path = dir.join(format!("bd_tools-{}.ts", fingerprint(SOURCE)));

    if !std::fs::read_to_string(&path).is_ok_and(|have| have == SOURCE) {
        std::fs::create_dir_all(&dir).map_err(|e| {
            format!(
                "cannot create {} for the typed bd tools: {e} \
                 (set LOOPRS_BD_TOOLS_EXT to a readable file, or LOOPRS_BEADS_WORKER_TOOLS=off \
                 to run the worker through bash)",
                dir.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Owner-only, and set on every start rather than only on create: a dir
            // that already existed at 0777 is exactly the case worth fixing.
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let tmp = dir.join(format!(
            ".bd_tools-{}.{}.tmp",
            fingerprint(SOURCE),
            std::process::id()
        ));
        std::fs::write(&tmp, SOURCE)
            .map_err(|e| format!("cannot write the typed bd tools to {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            format!(
                "cannot move the typed bd tools into {}: {e}",
                path.display()
            )
        })?;
    }
    let _ = CACHED.set(path.clone());
    Ok(path)
}

/// The tool names the embedded extension registers, read out of its own text.
///
/// This is the drift detector behind rule 1: [`TOOLS`] is what the knob and the
/// prompt believe, and this is what the extension says. Reading the text rather
/// than running the file keeps it a unit test — no node, no `pi`, no model call.
#[cfg(test)]
fn embedded_tools() -> Vec<String> {
    SOURCE
        .lines()
        .filter_map(|line| {
            // A registration is a `<name>: {` line in the schema table, so the name is
            // the first token of the line and nothing but a tool name gets through.
            let head = line
                .trim()
                .split([':', '(', ' ', '{'])
                .next()
                .unwrap_or("")
                .to_string();
            (head.starts_with("bd_") && head.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
                .then_some(head)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule the whole module exists to keep: the Rust half's tool list and the
    /// extension's registrations are the same five names, in the same order.
    #[test]
    fn embedded_schema_names_the_same_tools() {
        assert_eq!(
            embedded_tools(),
            TOOLS.to_vec(),
            "the extension's registered tools and this module's TOOLS have drifted apart"
        );
    }

    #[test]
    fn the_default_names_pi_s_builtins_plus_the_three_verbs_a_pass_needs() {
        let surface = Surface::new(DEFAULT_WORKER_TOOLS, Source::Embedded);
        assert_eq!(
            surface.names(),
            vec!["bd_show", "bd_close", "bd_remember"],
            "the default is the three verbs the worker prompt actually tells it to use, in the \
             extension's own order — and deliberately not bd_ready/bd_claim"
        );
        assert!(
            !surface.has("claim"),
            "the loop claims; the worker does not"
        );
        assert!(
            !surface.has("ready"),
            "the loop picks the work; the worker does not shop"
        );
    }

    #[test]
    fn the_bash_path_is_no_argv_at_all() {
        assert_eq!(Surface::off().spawn_args().unwrap(), Vec::<String>::new());
        assert!(Surface::off().is_off());
        let no_bd_names = Surface::new("read,bash,edit,write", Source::Embedded);
        assert!(
            no_bd_names.is_off(),
            "a list with no `bd_*` name loads no extension at all: no schema, no cost"
        );
        assert_eq!(no_bd_names.spawn_args().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_named_extension_that_is_not_there_refuses_the_pass_and_names_the_knob() {
        let surface = Surface::new(
            DEFAULT_WORKER_TOOLS,
            Source::Path(PathBuf::from("/nope/x.ts")),
        );
        let err = surface.spawn_args().unwrap_err();
        assert!(
            err.contains("LOOPRS_BD_TOOLS_EXT") && err.contains("/nope/x.ts"),
            "the refusal has to name the knob and the path: {err}"
        );
    }

    #[test]
    fn spawn_argv_is_extension_then_tools_and_names_every_tool() {
        let args = Surface::new("read,bash,bd_close", Source::Embedded)
            .spawn_args()
            .unwrap();
        assert_eq!(args[0], "-e");
        assert_eq!(args[2], "--tools");
        assert_eq!(args[3], "read,bash,bd_close");
        // pi 1.0.0: a `+name` delta list declares nothing at all, so the list has
        // to be plain names. Cheap to assert, and invisible when it breaks.
        assert!(
            !args[3].contains('+'),
            "the delta form is not usable in pi 1.0.0 — name every tool"
        );
    }

    #[test]
    fn the_embedded_copy_materialises_to_what_shipped() {
        let surface = Surface::new(DEFAULT_WORKER_TOOLS, Source::Embedded);
        let path = surface
            .extension()
            .expect("the embedded copy writes somewhere");
        assert_eq!(
            std::fs::read_to_string(&path).expect("and it is readable"),
            SOURCE,
            "what pi would load is not what this binary embeds"
        );
        assert_eq!(
            surface.extension().unwrap(),
            path,
            "the second call must not re-write or move the file"
        );
        // Content-addressed: a different version is a different filename, so what
        // an older looprs wrote cannot be picked up by this one.
        assert!(path.to_string_lossy().contains(&fingerprint(SOURCE)));
    }

    #[test]
    fn fingerprint_moves_when_the_source_moves() {
        assert_ne!(fingerprint(SOURCE), fingerprint(&format!("{SOURCE}\n")));
    }

    #[test]
    fn the_shell_verbs_are_not_tools() {
        assert_eq!(verb_of("bash"), None);
        assert_eq!(verb_of("bd_"), None);
        assert_eq!(verb_of("bd_show"), Some("show"));
    }
}
