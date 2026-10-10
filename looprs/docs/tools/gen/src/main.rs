//! `looprs-book` — the static docs site generator (looprs-00u.2, ADR-0008).
//!
//! Markdown in, HTML out. Deliberately small: the two things it does that a plain
//! `pandoc` loop does not are (a) **path mapping**, so that the relative links the
//! corpus already contains (`../../src/services/bd.rs`,
//! `../../spikes/results/clipboard-cost.log`, `../tests/fixtures`) resolve inside
//! the built site without a single one of them being edited, and (b) **heading
//! anchors that match the GitHub slugger**, because `adr/0007-kanban-board.md` is
//! linked by section (`#1-the-status--column-mapping`) from three places and an
//! anchor that does not match is a link that silently lands at the top of the page.
//!
//! # The two roots
//!
//! * `docs/` is the book: a page at `docs/guide/keymap.md` becomes
//!   `<out>/guide/keymap.html`, and `docs/index.md` becomes `<out>/index.html`.
//! * the repo root is mirrored under `<out>/_repo/` **on demand**: only files a
//!   page actually links to are carried over. That is what keeps a link into
//!   `src/` (2.3 MB of Rust) from turning into a 2.3 MB copy of all of it, and it
//!   is why the whole site builds in well under a second.
//!
//! Every emitted link is *relative within the site*, so the site can be served from
//! any prefix — a subdirectory of a web server, GitHub Pages under `/looprs/`, or
//! `file://` straight off disk — with no base-URL configuration.
//!
//! # What it refuses to be
//!
//! No content policy lives here. The page list comes from `docs/SUMMARY.md`; the
//! prose comes from the `.md` files. The rot gate that decides whether the prose is
//! still true is `./scripts/docs_check.sh`, not this generator: a generator that
//! invented navigation would be a second index to keep in step with the first.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use pulldown_cmark::{html, Options, Parser};

/// A file larger than this is not carried into the site. Nothing in this repo is
/// close (`spikes/results/` — every captured log, all of it — is 304 KB), so the
/// cap exists only to stop a stray link at a 2 GB capture from being silently
/// copied on every build.
const MAX_MATERIALIZE_BYTES: u64 = 8 * 1024 * 1024;

/// A ceiling on the *total* carried into the site across every link. The current
/// corpus carries ~4 MB; this is deliberately generous and exists so that a
/// mistake in the link graph shows up as a warning naming the count rather than a
/// disk-full surprise.
const TOTAL_MATERIALIZE_BYTES: u64 = 64 * 1024 * 1024;

/// Bytes as a human said they would say them.
fn human(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{bytes} B")
    }
}

/// Directories a listing never descends into, and a link into never crawls.
///
/// The crawl is "everything a page links to", and a page linking at a directory
/// that contains `node_modules` (the TypeScript predecessor one directory over
/// does exactly that) turns a 0.7 s docs build into a 287 MB one. That happened,
/// which is why this list exists rather than a comment saying "be careful".
const EXCLUDE_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    "target",
    "_site",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    ".next",
];

/// Extensions that get a `<pre>` wrapper page rather than a byte copy.
const WRAPPED_EXTS: &[&str] = &[
    "rs", "toml", "py", "sh", "log", "txt", "json", "yaml", "yml", "md", "ts", "js", "html", "cfg",
    "ini", "diff", "patch",
];

/// A page in the site: a `.md` file, plus the nav entry that reached it.
#[derive(Clone)]
struct Node {
    title: String,
    /// Repo-absolute path of the page this entry points at, if it points at one.
    page: Option<PathBuf>,
    children: Vec<Node>,
}

/// The same thing while parsing, where children are indices into the flat node list.
struct RawNode {
    title: String,
    page: Option<PathBuf>,
    children: Vec<usize>,
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str, default: &str| -> String {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| default.to_string())
    };

    let cwd = std::env::current_dir().expect("cwd");
    let root = PathBuf::from(opt("--root", cwd.to_str().unwrap()));
    let book = PathBuf::from(opt("--book", "docs"));
    let out = PathBuf::from(opt("--out", "docs/_site"));
    let summar = opt("--summary", "docs/SUMMARY.md");
    let stamp = opt("--stamp", "generated");
    // Which trees this build *owns*. Warnings are reported only for pages under
    // these roots: a page carried in from a neighbouring project (the TypeScript
    // predecessor one directory over) has its own link hygiene, and its stale
    // links showing up in our build output teaches nobody anything.
    let corpus: Vec<PathBuf> = {
        let mut out = Vec::new();
        let mut i = 1;
        while let Some(pos) = args[i..].iter().position(|a| a == "--corpus") {
            let idx = i + pos + 1;
            if let Some(v) = args.get(idx) {
                out.push(abs(Path::new(v)));
            }
            i = idx;
        }
        if out.is_empty() {
            out.push(book.clone());
        }
        out
    };
    let root = abs(&root);
    let book = abs(&book);
    let out = abs(&out);

    match run(&root, &book, &out, &abs(Path::new(&summar)), &stamp, &corpus) {
        Ok(report) => {
            println!("wrote {}", out.display());
            println!(
                "  {} page(s), {} supporting file(s), {} external link(s)",
                report.pages, report.materialized, report.external
            );
            if report.stale > 0 {
                println!(
                    "  removed {} stale page(s): the site now contains only what this build wrote",
                    report.stale
                );
            }
            if !report.warnings.is_empty() {
                println!("  {} warning(s):", report.warnings.len());
                for w in report.warnings {
                    println!("   - {w}");
                }
            }
        }
        Err(e) => {
            eprintln!("looprs-book: {e}");
            std::process::exit(1);
        }
    }
}

struct Report {
    pages: usize,
    materialized: usize,
    external: usize,
    warnings: Vec<String>,
    stale: usize,
}

fn run(
    root: &Path,
    book: &Path,
    out: &Path,
    summar: &Path,
    stamp: &str,
    corpus: &[PathBuf],
) -> Result<Report, String> {
    let owns = |p: &Path| corpus.iter().any(|c| p.starts_with(c));
    let summar_text = read(summar)?;
    let nav = parse_summary(root, book, summar, &summar_text)?;

    // Seed the crawl with the pages the summary lists. Everything else in the site
    // arrives because something links to it: a `.md` link pulls that page in, a
    // link at `../../src/services/bd.rs` pulls that file in, a link at a
    // directory pulls the directory's contents in. The closure is the corpus —
    // which is exactly why an unlinked file costs nothing and a linked one cannot
    // 404.
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    collect_pages(&nav, &mut queue);

    let mut pages: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut materialized: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut external = 0usize;
    let mut total_bytes = 0u64;

    while let Some(target) = queue.pop_front() {
        if pages.contains_key(&target) || materialized.contains_key(&target) {
            continue;
        }
        if !target.exists() {
            warnings.push(format!("nothing at {}", show(root, &target)));
            continue;
        }
        let site_rel = out_path(root, book, &target);
        if target.is_dir() {
            let listing = directory_listing(root, &target);
            let mut links: Vec<(String, PathBuf)> = Vec::new();
            // The listing's links are relative to the *directory*, so the page it
            // is rewritten for is that directory's own index, not its parent.
            let listing_page = target.join("index.html");
            let ctx = PageCtx {
                root,
                book,
                page: &listing_page,
                site_rel: &site_rel,
                report: owns(&target),
            };
            let listing = rewrite_links(&ctx, &listing, &mut links, &mut external, &mut warnings)?;
            materialized.insert(target.clone(), site_rel.clone());
            let (nav, here) = nav_html(root, book, &nav, &site_rel);
            write_page(out, &site_rel, &listing, &nav, &here, stamp)?;
            queue.extend(links.into_iter().map(|(_, p)| p).filter(|p| !p.as_os_str().is_empty()));
            continue;
        }

        let meta = match fs::metadata(&target) {
            Ok(m) => m,
            Err(e) => {
                // An unreadable link target is a warning, not a build failure: a
                // docs build that will not run because one file in a chain is
                // unreadable takes the whole site down for one bad path.
                warnings.push(format!("cannot stat {}: {e}", show(root, &target)));
                continue;
            }
        };
        total_bytes += meta.len();
        if total_bytes > TOTAL_MATERIALIZE_BYTES {
            warnings.push(format!(
                "carrying nothing further into the site: the carried files have passed {} \
                 across {} entries. Check the links — a docs corpus this size is not \
                 the intent",
                human(TOTAL_MATERIALIZE_BYTES),
                materialized.len()
            ));
            break;
        }
        if meta.len() > MAX_MATERIALIZE_BYTES {
            warnings.push(format!(
                "not carried into the site ({} bytes, over the cap): {}",
                meta.len(),
                show(root, &target)
            ));
            continue;
        }

        if extension(&target).as_deref() == Some("md") {
            let text = read(&target)?;
            let body = render_page_markdown(&show(root, &target), &text)?;
            let mut links: Vec<(String, PathBuf)> = Vec::new();
            let ctx = PageCtx {
                root,
                book,
                page: &target,
                site_rel: &site_rel,
                report: owns(&target),
            };
            let body = rewrite_links(&ctx, &body, &mut links, &mut external, &mut warnings)?;
            pages.insert(target.clone(), site_rel.clone());
            let (nav, here) = nav_html(root, book, &nav, &site_rel);
            write_page(out, &site_rel, &body, &nav, &here, stamp)?;
            queue.extend(links.into_iter().map(|(_, p)| p).filter(|p| !p.as_os_str().is_empty()));
            continue;
        }

        // Not markdown. Either a text file worth a readable page (a `.rs`, a
        // `.py`, a captured `.log`) or a binary that is only ever *downloaded*.
        // Note that neither kind is crawled: a Rust file's `[`Foo`](crate::a::Foo)`
        // intra-doc links are Rust's namespace, not paths, and treating them as
        // paths turns every source file into a page full of false broken links.
        let wrapped = extension(&target)
            .map(|e| WRAPPED_EXTS.contains(&e.as_str()))
            .unwrap_or(false);
        if wrapped {
            materialized.insert(target.clone(), site_rel.clone());
            if let Err(e) = materialize_file(out, &site_rel, &target) {
                warnings.push(e);
            }
        } else {
            materialized.insert(target.clone(), site_rel.clone());
            let dest = out.join(&site_rel);
            if let Some(parent) = dest.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    warnings.push(format!("cannot create {}: {e}", parent.display()));
                    continue;
                }
            }
            if let Err(e) = fs::copy(&target, &dest) {
                warnings.push(format!("cannot copy {}: {e}", show(root, &target)));
            }
        }
    }

    if pages.is_empty() {
        return Err(format!(
            "{} listed no pages; the site would be empty",
            summar.display()
        ));
    }

    write_assets(out)?;

    // A page in the site with no page behind it is a page that is lying, and it is
    // lying in the one way this ADR says the build must not: `docs/_site` is not
    // committed precisely so that generated output cannot disagree with its source
    // while looking authoritative — but a file left in the output directory by an
    // earlier build disagrees just as quietly. (It happened: the pre-table build's
    // pages sat under `_site/_repo/` after the generator was fixed, so the site
    // that looked rebuilt still rendered 32 pages of pipe syntax.) The build knows
    // every path it wrote, so it deletes every `.html` it did not write.
    let keep: BTreeSet<String> = pages.values().chain(materialized.values()).cloned().collect();
    let stale = sweep_stale_pages(out, &keep);

    Ok(Report {
        pages: pages.len(),
        materialized: materialized.len(),
        external,
        warnings,
        stale: stale.len(),
    })
}

/// Delete `.html` files the build did not write. Returns what it deleted.
///
/// Deliberately limited to `.html` inside the output directory: the generator owns
/// the pages it generates and nothing else, and a sweep that deletes a stranger's
/// file is a worse bug than the stale page it removed.
fn sweep_stale_pages(out: &Path, keep: &BTreeSet<String>) -> Vec<String> {
    let mut doomed = Vec::new();
    let mut stack = vec![out.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let rel = match path.strip_prefix(out) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if !keep.contains(&rel) {
                doomed.push((path, rel));
            }
        }
    }
    let mut removed = Vec::new();
    for (path, rel) in doomed {
        if fs::remove_file(&path).is_ok() {
            removed.push(rel);
        }
    }
    removed.sort();
    removed
}

// ───────────────────────────── paths ─────────────────────────────

fn abs(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .expect("cwd")
            .join(p)
    }
}

fn extension(p: &Path) -> Option<String> {
    p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase())
}

fn show(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .to_string()
}

/// Join `base` and `href` without touching the filesystem, resolving `.`/`..`
/// textually. Needed because a link may point at something that does not exist, and
/// the caller has to be able to report *which* path it tried.
fn resolve(base_dir: &Path, href: &str) -> PathBuf {
    let p = Path::new(href);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let mut parts: Vec<Component<'_>> = base_dir.components().collect();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                parts.pop();
            }
            Component::CurDir => {}
            other => parts.push(other),
        }
    }
    parts
        .iter()
        .map(|c| c.as_os_str())
        .collect::<PathBuf>()
}

/// The repo-mirror path of a file that is copied through unchanged (no `.html`).
fn mirror_path(root: &Path, target: &Path) -> String {
    format!(
        "_repo/{}",
        target
            .strip_prefix(root)
            .unwrap_or(target)
            .to_string_lossy()
            .replace('\\', "/")
    )
}

/// Where a repo-absolute path lands inside the site.
fn site_path(root: &Path, book: &Path, target: &Path) -> String {
    if let Ok(rel) = target.strip_prefix(book) {
        return page_rel(rel, false);
    }
    let rel = target.strip_prefix(root).unwrap_or(target);
    page_rel(rel, true)
}

/// Where a repo path is *written* in the site — and, the same string, how a link to
/// it is spelled. One function for both on purpose: the two used to be computed
/// separately and disagreed on every file that was copied rather than rendered, so
/// the link said `foo.raw.html` while the file landed at `foo.raw`.
fn out_path(root: &Path, book: &Path, p: &Path) -> String {
    if p.is_dir() {
        return site_path(root, book, p);
    }
    let ext = extension(p).unwrap_or_default();
    if ext == "md" || WRAPPED_EXTS.contains(&ext.as_str()) {
        site_path(root, book, p)
    } else {
        mirror_path(root, p)
    }
}

fn page_rel(rel: &Path, under_repo_mirror: bool) -> String {
    let s = rel.to_string_lossy().replace('\\', "/");
    let s = if s.ends_with(".md") {
        format!("{}.html", &s[..s.len() - 3])
    } else if s.ends_with('/') || !s.contains('.') {
        format!("{s}/index.html")
    } else {
        format!("{s}.html")
    };
    if under_repo_mirror {
        // A file outside the repo arrives absolute (`/Users/…` on the machine that
        // wrote the link, `/workspace/…` in a container). `_repo/` joined to an
        // absolute string is `_repo//workspace/…`: `Path::join` collapses the
        // double slash when it writes and does **not** collapse it when something
        // compares the strings, so a build that asks "which paths did I write?"
        // answers wrong for every out-of-repo carry. Normalise at the one place the
        // mirror name is built.
        format!("_repo/{}", s.trim_start_matches('/'))
    } else {
        s
    }
}

/// The relative href from one site path to another site path. Computed inside the
/// site, never from the repo, so the emitted link is correct wherever the site is
/// served from.
fn site_link(from: &str, to: &str) -> String {
    let from_dir = Path::new(from)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let from_parts: Vec<&str> = if from_dir.is_empty() {
        Vec::new()
    } else {
        from_dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    let to_parts: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let mut common = 0;
    while common < from_parts.len()
        && common < to_parts.len()
        && from_parts[common] == to_parts[common]
    {
        common += 1;
    }
    let mut out = Vec::new();
    for _ in common..from_parts.len() {
        out.push("..".to_string());
    }
    out.extend(to_parts[common..].iter().map(|s| s.to_string()));
    if out.is_empty() {
        ".".to_string()
    } else {
        out.join("/")
    }
}

// ─────────────────────────── summary parsing ───────────────────────────

/// Parse a `SUMMARY.md` nested bullet list into the nav tree.
///
/// A `[text](path)` entry is a page; a line with no link is a **section heading**
/// (`* **Reference**`), which groups its children and points at nothing. The
/// `# Summary` title is dropped.
fn parse_summary(root: &Path, book: &Path, summar: &Path, text: &str) -> Result<Vec<Node>, String> {
    let dir = summar.parent().unwrap_or(book).to_path_buf();
    // stack of (indent, node index), so nesting comes from the bullet indentation
    // and nothing else.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut nodes: Vec<RawNode> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("# ") {
            continue;
        }
        let indent = line.len() - trimmed.len();
        let Some(rest) = trimmed.strip_prefix('*').or_else(|| trimmed.strip_prefix('-')) else {
            continue;
        };
        let rest = rest.trim_start();
        let (title, href) = split_link(rest);
        let page = match href {
            Some(h) => {
                let bare = h.split('#').next().unwrap_or("");
                let target = resolve(&dir, bare);
                let target = abs(&target);
                if !target.exists() {
                    return Err(format!(
                        "SUMMARY.md: no such page for {:?} (looked at {})",
                        h,
                        show(root, &target)
                    ));
                }
                Some(target)
            }
            None => None,
        };
        let node = RawNode {
            title,
            page,
            children: Vec::new(),
        };
        let idx = nodes.len();
        nodes.push(node);
        while let Some(&(i, _)) = stack.last() {
            if i < indent {
                break;
            }
            stack.pop();
        }
        if let Some(&(_, parent)) = stack.last() {
            nodes[parent].children.push(idx);
        }
        stack.push((indent, idx));
    }

    // Flatten the "roots" trick: rebuild as a forest of node indices.
    let mut top: Vec<usize> = Vec::new();
    {
        let mut child_of: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, n) in nodes.iter().enumerate() {
            for c in &n.children {
                child_of.insert(*c, vec![i]);
            }
        }
        for i in 0..nodes.len() {
            if !child_of.contains_key(&i) {
                top.push(i);
            }
        }
    }
    Ok(reindex(&nodes, &top))
}

/// Copy the tree out of the flat parse into owned `Node`s.
fn reindex(nodes: &[RawNode], top: &[usize]) -> Vec<Node> {
    top.iter()
        .map(|&i| Node {
            title: nodes[i].title.clone(),
            page: nodes[i].page.clone(),
            children: reindex(nodes, &nodes[i].children),
        })
        .collect()
}

fn split_link(s: &str) -> (String, Option<String>) {
    if let Some(open) = s.find('[') {
        if let Some(close) = s[open..].find(']') {
            let title = s[open + 1..open + close].trim().to_string();
            let after = &s[open + close + 1..];
            if let Some(start) = after.find('(') {
                if let Some(end) = after[start..].find(')') {
                    return (title, Some(after[start + 1..start + end].trim().to_string()));
                }
            }
            return (title, None);
        }
    }
    (s.trim().trim_matches('*').trim().to_string(), None)
}

fn collect_pages(nodes: &[Node], q: &mut VecDeque<PathBuf>) {
    for n in nodes {
        if let Some(p) = &n.page {
            q.push_back(p.clone());
        }
        collect_pages(&n.children, q);
    }
}

// ───────────────────────────── rendering ─────────────────────────────

fn render_markdown(md: &str) -> String {
    let mut out = String::new();
    // The extension options are load-bearing, not decoration. `Parser::new` means
    // *no* extensions, and a pipe table under a parser with no `ENABLE_TABLES` does
    // not fail: it renders as a paragraph of pipes. That is what shipped, site-wide
    // — every table row in the corpus (1,024 when this was counted; re-count with the
    // `grep -rc '^|'` one-liner in ADR-0008's amendment) came out as literal `| … |`
    // text, which is unreadable on a desktop and wide enough on a phone to push the
    // page into horizontal scroll. `unrendered_tables` is the gate that stops it recurring.
    //
    // The set mirrors (and deliberately exceeds by TABLES) what the app's own
    // markdown renderer turns on in `src/utils/md.rs`.
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    html::push_html(&mut out, Parser::new_ext(md, opts));
    add_heading_ids(&out)
}

/// Render a page's markdown, refusing the page if markdown syntax survived.
///
/// The only failure mode of a markdown renderer worth fearing is the silent one:
/// output that is *valid HTML* and wrong prose. A table that did not render is not
/// ugly, it is source code wearing a paragraph, and it is both invisible in a diff
/// of the HTML (nothing is committed) and obvious to a reader too late.
fn render_page_markdown(what: &str, md: &str) -> Result<String, String> {
    let rendered = render_markdown(md);
    match unrendered_tables(&rendered) {
        empty if empty.is_empty() => Ok(rendered),
        hits => Err(format!(
            "{what}: markdown table syntax survived into the HTML ({} line(s), first: {}). \
             Either `render_markdown`'s Options no longer include TABLES, or this table's \
             header/`|---|` delimiter pair is malformed or glued to the paragraph above it \
             (a table must start a block: one blank line before it).",
            hits.len(),
            hits[0]
        )),
    }
}

/// Prose lines of rendered HTML that are still markdown pipe-table syntax.
///
/// Checked on the output rather than the source because the output is the artefact
/// the reader sees. Two exclusions make the check precise, and both were learned
/// the hard way:
///
/// * **code regions** — an inline `` `grep -E 'logging: |kanban|clipboard'` `` has
///   pipes at the start of a wrapped line in `docs/guide/configuration.md`, and a
///   check that ignores code spans calls that a broken table. ASCII figures in
///   fenced blocks drop out for the same reason (and their `│` is not `|` anyway).
/// * **real tables** — the clipboard knob's table has cells that quote
///   `` `auto` \| `native` \| `off` ``, so a *correctly rendered* row contains
///   pipes in its text. Excluding `<table>…</table>` is what leaves a check for
///   pipes that escaped a table rather than pipes that are inside one.
///
/// What survives is markdown pipe syntax sitting in prose, which has exactly two
/// causes: extension options that no longer include `ENABLE_TABLES`, or a table
/// with no blank line above it (CommonMark will not start a table while a paragraph
/// is still running, and nothing about that failure is loud).
fn unrendered_tables(rendered: &str) -> Vec<String> {
    prose_text(rendered)
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('|') && line.matches('|').count() >= 3)
        .map(|line| format!("`{}`", line.chars().take(60).collect::<String>()))
        .collect()
}

/// The rendered HTML with code spans, code blocks and real tables removed, and its
/// tags stripped: the prose a reader gets.
fn prose_text(html: &str) -> String {
    let mut regions = CodeRegions::new(html).ranges;
    regions.extend(table_regions(html));
    regions.sort();
    let mut kept = String::with_capacity(html.len());
    let mut cursor = 0usize;
    for (start, end) in &regions {
        if *start > cursor {
            kept.push_str(&html[cursor..*start]);
        }
        cursor = (*end).min(html.len()).max(cursor);
    }
    kept.push_str(&html[cursor..]);
    strip_tags(&kept)
}

/// Byte ranges of the rendered HTML that are a rendered table.
fn table_regions(html: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(i) = html[cursor..].find("<table") {
        let start = cursor + i;
        let Some(j) = html[start..].find("</table>") else { break };
        let end = start + j + "</table>".len();
        out.push((start, end));
        cursor = end;
    }
    out
}

/// Add `id="..."` to every heading, using GitHub's slug rules.
///
/// GitHub's rule is: lowercase, spaces to `-`, drop everything that is not
/// alphanumeric or `-`. It does **not** collapse repeated hyphens, which is why
/// `## 1. The status → column mapping` is `1-the-status--column-mapping` — the
/// arrow contributes nothing and the two spaces around it leave two hyphens
/// behind. `adr/0007` is linked by that anchor from three files, so this is not
/// a detail.
fn add_heading_ids(html: &str) -> String {
    let guard = CodeRegions::new(html);
    let mut out = String::with_capacity(html.len() + 256);
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut cursor = 0usize;
    while let Some(start) = find_heading_tag(html, cursor, &guard) {
        out.push_str(&html[cursor..start]);
        let close = match html[start..].find('>') {
            Some(c) => start + c,
            None => {
                out.push_str(&html[start..]);
                return out;
            }
        };
        let level = html.as_bytes()[start + 2] as char;
        let end_tag = format!("</h{level}>");
        let body_start = close + 1;
        let body_end = rest_of(html, body_start)
            .find(&end_tag)
            .map(|i| body_start + i)
            .unwrap_or(html.len());
        let text = strip_tags(&html[body_start..body_end]);
        let mut slug = slugify(&text);
        if let Some(n) = seen.get(&slug) {
            slug = format!("{slug}-{}", n + 1);
            seen.insert(slug.clone(), n + 1);
        } else {
            seen.insert(slug.clone(), 0);
        }
        out.push_str(&format!("<h{level} id=\"{slug}\">"));
        cursor = close + 1;
    }
    out.push_str(&html[cursor..]);
    out
}

fn rest_of(hay: &str, from: usize) -> &str {
    &hay[from..]
}

/// Find the next real `<hN>` opening tag at or after `from`, skipping anything
/// inside a `<pre>`/`<code>` region — a doc that shows HTML in a code fence is
/// not a doc that wants its `<html>` given an `id`.
fn find_heading_tag(hay: &str, from: usize, guard: &CodeRegions) -> Option<usize> {
    let mut at = from;
    loop {
        let rel = hay[at..].find("<h")?;
        let pos = at + rel;
        let bytes = hay.as_bytes();
        let level = *bytes.get(pos + 2)?;
        let is_heading = (b'1'..=b'6').contains(&level)
            && matches!(bytes.get(pos + 3), Some(b'>') | Some(b' '));
        if is_heading && !guard.inside(pos) {
            return Some(pos);
        }
        at = pos + 2;
    }
}

/// Byte ranges occupied by `<pre>…</pre>` and `<code>…</code>` in rendered HTML.
///
/// Both passes that mutate the rendered string — heading ids and link rewriting —
/// must not treat text inside a code fence as markup. This repo's docs quote HTML,
/// `ratatui` buffers, and terminal escape sequences inside fences, so "skip the
/// code" is not a theoretical concern here.
struct CodeRegions {
    ranges: Vec<(usize, usize)>,
}

impl CodeRegions {
    fn new(html: &str) -> Self {
        let mut ranges = Vec::new();
        let mut cursor = 0usize;
        loop {
            let open = ["<pre", "<code"]
                .iter()
                .filter_map(|tok| html[cursor..].find(*tok).map(|i| (cursor + i, *tok)))
                .min_by_key(|(i, _)| *i);
            let Some((pos, tok)) = open else { break };
            let close_tok = if tok == "<pre" { "</pre>" } else { "</code>" };
            let end = html[pos..]
                .find(close_tok)
                .map(|i| pos + i + close_tok.len())
                .unwrap_or(html.len());
            ranges.push((pos, end));
            cursor = end;
        }
        Self { ranges }
    }

    fn inside(&self, pos: usize) -> bool {
        self.ranges.iter().any(|(a, b)| pos >= *a && pos < *b)
    }
}

fn slugify(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            out.push('-');
        } else if ch == '_' || ch == '-' || ch.is_alphanumeric() {
            for l in ch.to_lowercase() {
                out.push(l);
            }
        }
    }
    if out.is_empty() {
        "section".to_string()
    } else {
        out
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
}

/// Rewrite every `href="…"` in rendered HTML.
///
/// Returns the rewritten body plus the repo-absolute paths the links pointed at, so
/// the caller can crawl them. `links` holds `(raw_href, resolved_path_string)`.
/// Everything link rewriting needs to know about the page being rendered.
///
/// Bundled rather than passed as eight arguments because `clippy::too_many_arguments`
/// is right that a nine-argument function is a bug magnet, and the bundle is the
/// honest shape: it is one page's identity, its place in the site, and whether this
/// build reports problems found in it.
struct PageCtx<'a> {
    root: &'a Path,
    book: &'a Path,
    page: &'a Path,
    site_rel: &'a str,
    /// Whether this build reports warnings for this page. `false` for pages carried
    /// in from outside the corpus, whose link hygiene belongs to whoever wrote them.
    report: bool,
}

impl<'a> PageCtx<'a> {
    fn dir(&self) -> PathBuf {
        self.page.parent().unwrap_or(self.root).to_path_buf()
    }
}

fn rewrite_links(
    ctx: &PageCtx<'_>,
    body: &str,
    links: &mut Vec<(String, PathBuf)>,
    external: &mut usize,
    warnings: &mut Vec<String>,
) -> Result<String, String> {
    let root = ctx.root;
    let book = ctx.book;
    let page = ctx.page;
    let site_rel = ctx.site_rel;
    let report = ctx.report;
    let dir = ctx.dir();
    let guard = CodeRegions::new(body);
    let mut out = String::with_capacity(body.len() + 256);
    let mut cursor = 0usize;
    while let Some(pos) = find_href(body, cursor, &guard) {
        out.push_str(&body[cursor..pos]);
        let after = &body[pos + "href=\"".len()..];
        let Some(end) = after.find('"') else {
            out.push_str(&body[pos..]);
            return Ok(out);
        };
        let raw = &after[..end];
        let tag_end = pos + "href=\"".len() + end + 1;
        cursor = tag_end;

        if raw.is_empty() {
            if report {
                warnings.push(format!("empty href in {}", show(root, page)));
            }
            out.push_str(&format!("href=\"{raw}\""));
            continue;
        }

        let lowered = raw.to_ascii_lowercase();
        if lowered.starts_with("http://")
            || lowered.starts_with("https://")
            || lowered.starts_with("mailto:")
            || raw.starts_with('#')
        {
            if !raw.starts_with('#') {
                *external += 1;
                out.push_str(&format!(
                    "href=\"{raw}\" target=\"_blank\" rel=\"noopener noreferrer\" class=\"external\""
                ));
            } else {
                out.push_str(&format!("href=\"{raw}\""));
            }
            continue;
        }

        // Relative: split off the anchor, resolve the target against the *page's*
        // directory in the repo, then re-express the link inside the site.
        let (target_raw, anchor) = match raw.find('#') {
            Some(i) => (&raw[..i], &raw[i..]),
            None => (raw, ""),
        };
        if target_raw.is_empty() {
            out.push_str(&format!("href=\"{raw}\""));
            continue;
        }
        let resolved = resolve(&dir, target_raw);
        if !resolved.exists() {
            // Left untouched: the page still builds, the link still shows, and the
            // gate is the one that says "this is broken".
            if report {
                warnings.push(format!(
                    "broken link in {}: {raw} → {}",
                    show(root, page),
                    show(root, &resolved)
                ));
            }
            out.push_str(&format!("href=\"{raw}\""));
            continue;
        }
        let resolved_abs = abs(&resolved);
        let target_site = out_path(root, book, &resolved_abs);
        let href = site_link(site_rel, &target_site);
        out.push_str(&format!("href=\"{href}{anchor}\""));
        links.push((raw.to_string(), resolved_abs));
    }
    out.push_str(&body[cursor..]);
    Ok(out)
}

fn find_href(hay: &str, from: usize, guard: &CodeRegions) -> Option<usize> {
    let mut at = from;
    loop {
        let rel = hay[at..].find("href=\"")?;
        let pos = at + rel;
        if !guard.inside(pos) {
            return Some(pos);
        }
        at = pos + 5;
    }
}


/// A page that lists a directory's contents, for links like `../tests/fixtures`.
fn directory_listing(root: &Path, dir: &Path) -> String {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| {
                    let p = e.path();
                    let is_dir = p.is_dir();
                    (p, is_dir)
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by_key(|(p, is_dir)| (!is_dir, p.to_string_lossy().to_string()));
    let mut md = format!(
        "# `{}`\n\nContents of this directory, carried into the site so the link that \
         pointed here lands somewhere.\n\n",
        show(root, dir)
    );
    let mut skipped = 0usize;
    for (p, is_dir) in entries {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        if is_dir {
            if EXCLUDE_DIRS.contains(&name.as_str()) {
                skipped += 1;
                continue;
            }
            // Directories are listed but NOT linked: the crawl that carries files in
            // is the same crawl that would otherwise follow a link into someone's
            // dependency tree. A reader who wants a subdirectory has it in the repo.
            md.push_str(&format!("* 📁 `{name}/` *(directory, not carried in)*\n"));
        } else {
            md.push_str(&format!("* 📄 [`{name}`]({name})\n"));
        }
    }
    if skipped > 0 {
        md.push_str(&format!("\n_{skipped} excluded directory(ies) (node_modules, .git, target, …)_\n"));
    }
    render_markdown(&md)
}

/// A text/code file becomes a `<pre>` page so it is readable in a browser without
/// downloading it. The bytes are escaped and otherwise unmodified: this is the
/// same file `git show` prints, in a page with a sidebar.
fn materialize_file(out: &Path, site_rel: &str, file: &Path) -> Result<(), String> {
    let bytes = fs::read(file).map_err(|e| format!("{file:?}: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let name = file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let body = format!(
        "<h1 id=\"file\"><code>{}</code></h1>\n<p class=\"srcnote\">A repository file carried into \
         the site unchanged so the documentation link that pointed at it lands. Lines: {}. \
         Bytes: {}.</p>\n<pre class=\"source\"><code>{}</code></pre>\n",
        escape(&name),
        text.lines().count(),
        bytes.len(),
        escape(&text)
    );
    write_raw(out, site_rel, &body)
}

fn write_raw(out: &Path, site_rel: &str, body: &str) -> Result<(), String> {
    let path = out.join(site_rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{parent:?}: {e}"))?;
    }
    let depth = site_rel.matches('/').count();
    let up = if depth == 0 {
        ".".to_string()
    } else {
        vec![".."; depth].join("/")
    };
    let page = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>looprs · source</title>\n\
         <link rel=\"stylesheet\" href=\"{up}/assets/book.css\">\n\
         <script src=\"{up}/assets/livereload.js\" defer></script>\n\
         </head>\n<body>\n\
         <header class=\"topbar\"><a class=\"brand\" href=\"{up}/index.html\">loop<span>rs</span></a>\
         <span class=\"tagline\">repository file, carried through the docs build</span>\
         <a class=\"home\" href=\"{up}/index.html\">docs home</a></header>\
         <nav class=\"sidebar\" aria-label=\"Docs\"></nav>\
         <main>\n<article>\n{body}\n</article>\
         </main>\n</body>\n</html>\n",
        up = up,
        body = body
    );
    fs::write(&path, page).map_err(|e| format!("{path:?}: {e}"))?;
    Ok(())
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

// ───────────────────────────── chrome ─────────────────────────────

/// The sidebar, re-rendered for every page.
///
/// Re-rendered rather than shared because every link in it has to be relative to
/// *that* page: the site is served from an unknown prefix, so an absolute href
/// would pin it to one mount point. The cost is a few KB of string work per page
/// across ~40 pages, which is not a thing worth caching.
///
/// The second half of the return is the title of the page being rendered — the
/// tree knows which entry it marked active, and on a phone the tree is folded
/// until you ask, so the fold's label has to say where you are.
fn nav_html(root: &Path, book: &Path, nodes: &[Node], current: &str) -> (String, String) {
    let mut out = String::new();
    let mut here = String::new();
    out.push_str("<ul class=\"nav depth-0\">");
    nav_items(&mut out, root, book, nodes, current, &mut here);
    out.push_str("</ul>");
    (out, here)
}

fn nav_items(
    out: &mut String,
    root: &Path,
    book: &Path,
    nodes: &[Node],
    current: &str,
    here: &mut String,
) {
    for n in nodes {
        match &n.page {
            Some(p) => {
                let target = site_path(root, book, p);
                let href = site_link(current, &target);
                let active = if target == current { " class=\"active\"" } else { "" };
                if target == current && here.is_empty() {
                    *here = n.title.clone();
                }
                out.push_str(&format!(
                    "<li><a{active} href=\"{href}\">{}</a></li>",
                    escape(&n.title)
                ));
            }
            None => {
                out.push_str(&format!("<li class=\"group\">{}", escape(&n.title)));
                if !n.children.is_empty() {
                    out.push_str("<ul>");
                    nav_items(out, root, book, &n.children, current, here);
                    out.push_str("</ul>");
                }
                out.push_str("</li>");
            }
        }
    }
}

fn write_page(
    out: &Path,
    site_rel: &str,
    body: &str,
    nav: &str,
    here: &str,
    stamp: &str,
) -> Result<(), String> {
    let path = out.join(site_rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{parent:?}: {e}"))?;
    }
    let depth = site_rel.matches('/').count();
    let up = if depth == 0 {
        ".".to_string()
    } else {
        vec![".."; depth].join("/")
    };
    // The fold's label: what the tree is, and — because a folded tree tells a
    // reader nothing about where they are — the page they are on.
    let here_html = if here.is_empty() {
        String::new()
    } else {
        format!(" <span class=\"navtoggle-here\">{}</span>", escape(here))
    };
    let page = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>looprs · docs</title>\n\
         <link rel=\"stylesheet\" href=\"{up}/assets/book.css\">\n\
         <script src=\"{up}/assets/livereload.js\" defer></script>\n\
         </head>\n<body>\n\
         <header class=\"topbar\"><a class=\"brand\" href=\"{up}/index.html\">loop<span>rs</span></a>\n\
         <span class=\"tagline\">a terminal that owns its transcript</span>\n\
         <a class=\"home\" href=\"{up}/index.html\">docs home</a></header>\n\
         <nav class=\"sidebar\" aria-label=\"Docs\">\
         <input class=\"navtoggle\" type=\"checkbox\" id=\"nav-toggle\" aria-label=\"Show the page tree\">\
         <label class=\"navtoggle-label\" for=\"nav-toggle\"><span>Contents</span>{here_html}</label>\
         {nav}</nav>\n\
         <main>\n<article>\n{body}\n</article>\n\
         <footer class=\"pagefooter\">Generated by <code>./scripts/docs.sh build</code> · {stamp}</footer>\n\
         </main>\n</body>\n</html>\n",
        up = up,
        body = body,
        stamp = escape(stamp),
        nav = nav,
        here_html = here_html
    );
    fs::write(&path, page).map_err(|e| format!("{path:?}: {e}"))?;
    Ok(())
}

fn write_assets(out: &Path) -> Result<(), String> {
    let dir = out.join("assets");
    fs::create_dir_all(&dir).map_err(|e| format!("{dir:?}: {e}"))?;
    fs::write(dir.join("book.css"), CSS).map_err(|e| format!("{dir:?}: {e}"))?;
    fs::write(dir.join("livereload.js"), LIVERELOAD).map_err(|e| format!("{dir:?}: {e}"))?;
    fs::write(out.join("__build"), stamp_of()).ok();
    Ok(())
}

fn stamp_of() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Polled by every page while `./scripts/docs.sh serve` is running; reloads the
/// page when the build token changes. Outside `serve` the fetch 404s, the counter
/// gives up after three misses and the script goes quiet — a built site read
/// straight off disk must not spend its life hammering a URL.
const LIVERELOAD: &str = r#"(() => {
  const el = document.currentScript;
  const root = el && el.src ? el.src.replace(/\/assets\/livereload\.js.*$/, "") : "";
  let token = null, misses = 0;
  async function tick() {
    if (misses > 3) return;
    try {
      const r = await fetch(root + "/__build?nocache=" + Date.now(), { cache: "no-store" });
      if (!r.ok) throw new Error(String(r.status));
      const t = (await r.text()).trim();
      misses = 0;
      if (token !== null && t !== token) { location.reload(); return; }
      token = t;
    } catch (e) { misses += 1; }
    setTimeout(tick, 700);
  }
  tick();
})();"#;

const CSS: &str = r##":root {
  --bg: #101216;
  --panel: #171a20;
  --fg: #d7dae0;
  --dim: #8b93a1;
  --accent: #7aa2f7;
  --code-bg: #0b0d11;
  --border: #262b34;
  --mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
  /* The two numbers the frame is measured in, each written once. --topbar-h is
     both the header's height and the offset the sidebar sticks under; stated
     twice they drift, and the drift is a strip of page hidden under an opaque
     header (which is how this sheet shipped for a while: the header's box was
     21px tall with 30px of text in it, and the sidebar stuck at a guessed 45px). */
  --topbar-h: 50px;
  --nav-w: 300px;
}
@media (prefers-color-scheme: light) {
  :root {
    --bg: #fbfbfd;
    --panel: #ffffff;
    --fg: #1c1f24;
    --dim: #5b6472;
    --accent: #2f5fd0;
    --code-bg: #f2f3f6;
    --border: #dcdfe5;
  }
}
* { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; text-size-adjust: 100%; }
html, body { margin: 0; padding: 0; background: var(--bg); color: var(--fg); }
body {
  font: 16px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  display: grid;
  grid-template-columns: var(--nav-w) minmax(0, 1fr);
  grid-template-rows: auto 1fr;
  grid-template-areas: "top top" "side main";
  min-height: 100vh;
  min-height: 100dvh;
}
/* A carried repository file has no place in the tree, so its sidebar is empty and
   the gutter beside it is a lie about width. Note the shape of the fix: the `side`
   area is **kept** and given zero width rather than dropped from the template.
   `check_layout_css` requires every `grid-area:` name to be named by every
   `grid-template-areas:` in the sheet, and an exemption here — "it is fine because
   another rule hides the item" — is the same loophole the mobile bug arrived
   through. The rule says what it says for every template. */
.sidebar:empty { display: none; }
body:has(.sidebar:empty) {
  grid-template-columns: minmax(0, 1fr);
  grid-template-areas: "top" "side" "main";
}
.topbar {
  grid-area: top;
  display: flex; align-items: center; gap: 14px;
  height: var(--topbar-h);
  padding: 0 20px;
  border-bottom: 1px solid var(--border);
  background: var(--panel);
  position: sticky; top: 0; z-index: 5;
}
.brand { font-family: var(--mono); font-size: 18px; font-weight: 700; color: var(--fg); text-decoration: none; }
.brand span { color: var(--accent); }
.tagline { color: var(--dim); font-size: 13px; flex: 1; }
.home { color: var(--accent); font-size: 13px; text-decoration: none; }
.sidebar {
  grid-area: side;
  border-right: 1px solid var(--border);
  background: var(--panel);
  padding: 14px 10px 40px 18px;
  overflow-y: auto;
  overscroll-behavior: contain;
  position: sticky;
  top: var(--topbar-h);
  max-height: calc(100vh - var(--topbar-h));
  max-height: calc(100dvh - var(--topbar-h));
  font-size: 14px;
}
ul.nav { list-style: none; margin: 0; padding-left: 0; }
ul.nav ul { list-style: none; margin: 2px 0 8px; padding-left: 14px; }
ul.nav li { margin: 3px 0; }
ul.nav li.group { color: var(--dim); text-transform: uppercase; letter-spacing: .06em; font-size: 11.5px; font-weight: 700; margin-top: 14px; }
ul.nav a { color: var(--fg); text-decoration: none; }
ul.nav a:hover { color: var(--accent); text-decoration: underline; }
ul.nav a.active { color: var(--accent); font-weight: 600; }
/* The phone disclosure. On a wide screen the tree is simply open and neither of
   these exists; on a phone the article is what you came for, so the tree starts
   folded and the checkbox unfolds it. CSS only, on purpose: a site that works
   from file:// has no script to lean on, and livereload.js is a `serve`-only
   file. The input stays in the tab order (transparent, not `display: none`) so
   the control is keyboard-operable and the label gets the focus ring. */
.navtoggle, .navtoggle-label { display: none; }
main { grid-area: main; padding: 26px 34px 80px; min-width: 0; }
article { max-width: 900px; overflow-wrap: break-word; }
/* Anchor jumps land under a sticky header unless every target says otherwise. */
[id] { scroll-margin-top: calc(var(--topbar-h) + 12px); }
article h1, article h2, article h3 { line-height: 1.25; margin: 1.9em 0 .55em; }
article h1 { font-size: 30px; margin-top: .3em; }
article h2 { font-size: 23px; border-bottom: 1px solid var(--border); padding-bottom: .25em; }
article h3 { font-size: 18.5px; }
article h4 { font-size: 16px; color: var(--dim); }
article a { color: var(--accent); }
article code {
  font-family: var(--mono); font-size: 87%;
  background: var(--code-bg); padding: .12em .35em; border-radius: 3px;
  border: 1px solid var(--border);
  /* A chip is an atom, not a run of text. As plain inline boxes, a long sequence
     of them (`LOOP_GIT_LOCK_WAIT_MS` / `LOOP_GIT_KILL_GRACE_MS` / …) laid out past
     the column edge in Chrome, and `overflow-wrap` in three spellings plus
     `word-break: break-all` all left the document at 379px on a 360px screen —
     measured on the carried TypeScript predecessor's README, the page that
     taught me this. As inline-blocks each chip moves to the next line whole, and
     `max-width` keeps one absurdly long chip inside the column instead of letting
     it punch out of it. */
  display: inline-block; max-width: 100%;
  overflow-wrap: anywhere;
}
article pre {
  background: var(--code-bg); border: 1px solid var(--border); border-radius: 6px;
  padding: 12px 14px; overflow-x: auto; max-width: 100%; line-height: 1.5;
}
article pre code { background: none; border: none; padding: 0; font-size: 13px; display: inline; max-width: none; overflow-wrap: normal; }
article blockquote {
  margin: 1em 0; padding: .4em 1em; border-left: 3px solid var(--accent);
  background: rgba(122,162,247,.06); color: var(--fg);
}
/* `display: block` on the table is what gives a wide table somewhere to go: as an
   inline table it has no scroll box, so a five-column reference either squeezes
   every cell into unreadability or makes the whole document wider than the
   screen. The cells still lay out as table cells inside it. */
article table { border-collapse: collapse; width: 100%; max-width: 100%; margin: 1.1em 0; font-size: 14.5px; display: block; overflow-x: auto; }
article th, article td { border: 1px solid var(--border); padding: 7px 10px; text-align: left; vertical-align: top; }
article th { background: var(--panel); font-weight: 700; }
article tr:nth-child(even) td { background: rgba(255,255,255,.012); }
article ul, article ol { padding-left: 22px; }
article li { margin: .3em 0; }
article img { max-width: 100%; height: auto; }
.pagefooter { margin-top: 60px; color: var(--dim); font-size: 12.5px; border-top: 1px solid var(--border); padding-top: 12px; }
.srcnote { color: var(--dim); font-size: 13px; }
pre.source { max-height: none; }
@media (max-width: 900px) {
  /* The invariant this block has to keep: every `grid-area:` name this sheet uses
     is named by the `grid-template-areas` here as well. Name a `.sidebar` at a
     `side` area that the narrow template does not define and the grid does not
     complain — it grows an implicit track for the ghost area and lays the header
     in one column beside it. That is what shipped: at 390px the body's tracks
     measured `348px 0px 42px`, so the header stopped 42px short of the right
     edge (a strip of bare page), the entire nav was crushed into that 42px, and
     the article was squeezed to 348px. `./scripts/docs_check.py` now fails the
     build when the two lists of names stop agreeing. */
  body {
    grid-template-columns: minmax(0, 1fr);
    grid-template-areas: "top" "side" "main";
  }
  .topbar { padding: 0 14px; gap: 10px; }
  .tagline { display: none; }
  .sidebar {
    position: static; max-height: none; overflow: visible;
    border-right: none; border-bottom: 1px solid var(--border);
    padding: 0 14px 8px;
  }
  .navtoggle {
    position: absolute; width: 1px; height: 1px; margin: 0;
    opacity: 0; pointer-events: none;
  }
  .navtoggle-label {
    display: flex; align-items: baseline; justify-content: space-between; gap: 10px;
    margin: 0 -14px; padding: 13px 14px;
    cursor: pointer; -webkit-tap-highlight-color: transparent;
    font-size: 12px; font-weight: 700; letter-spacing: .07em; text-transform: uppercase;
    color: var(--dim);
  }
  .navtoggle-label .navtoggle-here {
    text-transform: none; letter-spacing: 0; font-weight: 500;
    color: var(--fg); overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .navtoggle-label::after { content: "+"; font-size: 15px; color: var(--accent); }
  .navtoggle:checked ~ .navtoggle-label { border-bottom: 1px solid var(--border); }
  .navtoggle:checked ~ .navtoggle-label::after { content: "\2212"; }
  .navtoggle:focus-visible ~ .navtoggle-label { outline: 2px solid var(--accent); outline-offset: -3px; }
  .navtoggle:not(:checked) ~ ul.nav { display: none; }
  ul.nav { padding: 2px 0 10px; }
  /* Real tap targets: the tree is a finger's job at this width. */
  ul.nav li { margin: 0; }
  ul.nav a { display: block; padding: 7px 0; }
  ul.nav ul { padding-left: 12px; }
  main { padding: 18px 16px 60px; }
  article h1 { font-size: 25px; }
  article h2 { font-size: 20px; }
  article h3 { font-size: 17px; }
  .pagefooter { margin-top: 44px; }
}
"##;



