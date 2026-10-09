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

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};

use pulldown_cmark::{html, Parser};

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
            write_page(out, &site_rel, &listing, &nav_html(root, book, &nav, &site_rel), stamp)?;
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
            let body = render_markdown(&text);
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
            write_page(out, &site_rel, &body, &nav_html(root, book, &nav, &site_rel), stamp)?;
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
    Ok(Report {
        pages: pages.len(),
        materialized: materialized.len(),
        external,
        warnings,
    })
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
        format!("_repo/{s}")
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
    html::push_html(&mut out, Parser::new(md));
    add_heading_ids(&out)
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
fn nav_html(root: &Path, book: &Path, nodes: &[Node], current: &str) -> String {
    let mut out = String::new();
    out.push_str("<ul class=\"nav depth-0\">");
    nav_items(&mut out, root, book, nodes, current);
    out.push_str("</ul>");
    out
}

fn nav_items(out: &mut String, root: &Path, book: &Path, nodes: &[Node], current: &str) {
    for n in nodes {
        match &n.page {
            Some(p) => {
                let target = site_path(root, book, p);
                let href = site_link(current, &target);
                let active = if target == current { " class=\"active\"" } else { "" };
                out.push_str(&format!(
                    "<li><a{active} href=\"{href}\">{}</a></li>",
                    escape(&n.title)
                ));
            }
            None => {
                out.push_str(&format!("<li class=\"group\">{}", escape(&n.title)));
                if !n.children.is_empty() {
                    out.push_str("<ul>");
                    nav_items(out, root, book, &n.children, current);
                    out.push_str("</ul>");
                }
                out.push_str("</li>");
            }
        }
    }
}

fn write_page(out: &Path, site_rel: &str, body: &str, nav: &str, stamp: &str) -> Result<(), String> {
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
         <title>looprs · docs</title>\n\
         <link rel=\"stylesheet\" href=\"{up}/assets/book.css\">\n\
         <script src=\"{up}/assets/livereload.js\" defer></script>\n\
         </head>\n<body>\n\
         <header class=\"topbar\"><a class=\"brand\" href=\"{up}/index.html\">loop<span>rs</span></a>\n\
         <span class=\"tagline\">a terminal that owns its transcript</span>\n\
         <a class=\"home\" href=\"{up}/index.html\">docs home</a></header>\n\
         <nav class=\"sidebar\" aria-label=\"Docs\">{nav}</nav>\n\
         <main>\n<article>\n{body}\n</article>\n\
         <footer class=\"pagefooter\">Generated by <code>./scripts/docs.sh build</code> · {stamp}</footer>\n\
         </main>\n</body>\n</html>\n",
        up = up,
        body = body,
        stamp = escape(stamp),
        nav = nav
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
html, body { margin: 0; padding: 0; background: var(--bg); color: var(--fg); }
body {
  font: 16px/1.65 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  display: grid;
  grid-template-columns: 300px minmax(0, 1fr);
  grid-template-rows: auto 1fr;
  grid-template-areas: "top top" "side main";
  min-height: 100vh;
}
.topbar {
  grid-area: top;
  display: flex; align-items: baseline; gap: 14px;
  padding: 10px 20px;
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
  position: sticky;
  top: 45px;
  max-height: calc(100vh - 45px);
  font-size: 14px;
}
ul.nav { list-style: none; margin: 0; padding-left: 0; }
ul.nav ul { list-style: none; margin: 2px 0 8px; padding-left: 14px; }
ul.nav li { margin: 3px 0; }
ul.nav li.group { color: var(--dim); text-transform: uppercase; letter-spacing: .06em; font-size: 11.5px; font-weight: 700; margin-top: 14px; }
ul.nav a { color: var(--fg); text-decoration: none; }
ul.nav a:hover { color: var(--accent); text-decoration: underline; }
main { grid-area: main; padding: 26px 34px 80px; min-width: 0; }
article { max-width: 900px; }
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
}
article pre {
  background: var(--code-bg); border: 1px solid var(--border); border-radius: 6px;
  padding: 12px 14px; overflow-x: auto; line-height: 1.5;
}
article pre code { background: none; border: none; padding: 0; font-size: 13px; }
article blockquote {
  margin: 1em 0; padding: .4em 1em; border-left: 3px solid var(--accent);
  background: rgba(122,162,247,.06); color: var(--fg);
}
article table { border-collapse: collapse; width: 100%; margin: 1.1em 0; font-size: 14.5px; display: block; overflow-x: auto; }
article th, article td { border: 1px solid var(--border); padding: 7px 10px; text-align: left; vertical-align: top; }
article th { background: var(--panel); font-weight: 700; }
article tr:nth-child(even) td { background: rgba(255,255,255,.012); }
article ul, article ol { padding-left: 22px; }
article li { margin: .3em 0; }
.pagefooter { margin-top: 60px; color: var(--dim); font-size: 12.5px; border-top: 1px solid var(--border); padding-top: 12px; }
.srcnote { color: var(--dim); font-size: 13px; }
pre.source { max-height: none; }
@media (max-width: 900px) {
  body { grid-template-columns: 1fr; grid-template-areas: "top" "main"; }
  .sidebar { position: static; max-height: none; border-right: none; border-bottom: 1px solid var(--border); }
  main { padding: 18px; }
}
"##;


