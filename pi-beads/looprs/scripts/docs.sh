#!/usr/bin/env bash
# The docs build (looprs-00u.2 / ADR-0008).
#
# Three verbs:
#
#   ./scripts/docs.sh build   # build the site into docs/_site and print where it is
#   ./scripts/docs.sh serve   # build, then serve on http://localhost:3000 with live reload
#   ./scripts/docs.sh check   # the rot gate (links, knob coverage, keymap and
#                             wire-protocol coverage, measurement claims)
#
# Why a generator at all, and why this one: ADR-0008 Q1 priced `mdbook`, a
# hand-rolled generator and "markdown only on the git host" against each other.
# The deciding number was that the markdown parser this uses — `pulldown-cmark`
# 0.13.4 — is already in Cargo.lock, already compiled in this repo's `target/`,
# and already used by `src/utils/md.rs` to render markdown in the transcript.
# That makes a clean build of this site a ~1 second, zero-new-dependency,
# offline-capable operation, where `mdbook` needs a toolchain nobody here has
# installed and every contributor would have to install before previewing.
#
# The generated site is NOT committed (ADR-0008 Q3): it is ~1.4× the source in
# bytes and every prose edit would show up as a two-file diff. `docs/_site/` is
# gitignored; `./scripts/docs.sh build` is 1 second, so "read the site" means
# "run one command", not "trust a committed artefact".
#
# The generator is a separate cargo workspace (`docs/tools/gen`) sharing this
# repo's `target/`, so `cargo build` at the root still means "build the app" and
# nothing else.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
gen_manifest="$here/docs/tools/gen/Cargo.toml"
site="$here/docs/_site"
# Shared with the app on purpose: `pulldown-cmark` compiles once per repo, not
# once per package. Override CARGO_TARGET_DIR if you would rather not.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$here/target}"

die() {
    echo "docs.sh: $*" >&2
    exit "${2:-1}"
}

require() {
    command -v "$1" >/dev/null 2>&1 && return 0
    case "$1" in
        cargo)
            die "cargo is not on PATH. The docs generator is a Rust program.
       Install a toolchain:   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
       or, with Homebrew:     brew install rustup-init && brew install rust
       No network needed if you only want the rot gate:  ./scripts/docs.sh check"
            ;;
        python3)
            die "python3 is not on PATH — needed for 'serve' and 'check'.
       The build itself does not need it:  ./scripts/docs.sh build"
            ;;
        *) die "$1 is not on PATH" ;;
    esac
}

stamp() {
    # The stamp on every page footer. `git describe` when there is a tag, the short
    # sha otherwise, and a `-dirty` tail so a reader can tell the site was built
    # from a working tree rather than a commit. `--always` keeps this working in a
    # repo with no tags at all, which this one has none of yet.
    local sha date
    sha="$(git -C "$here" rev-parse --short --short HEAD 2>/dev/null || echo 'no-git')"
    date="$(git -C "$here" show -s --format=%cd --date=short HEAD 2>/dev/null || date -u +%Y-%m-%d)"
    local dirty=""
    if ! git -C "$here" diff --quiet -- docs spikes scripts 2>/dev/null; then
        dirty=" · built from a dirty tree"
    fi
    echo "$date · commit $sha$dirty"
}

build() {
    require cargo
    [ -f "$gen_manifest" ] || die "missing generator manifest at ${gen_manifest#"$here"/}"
    local t0 t1
    t0=$(date +%s)
    # `--offline` first: the fast path, and the honest one on a machine with no
    # network. If the dependency is genuinely not cached, retry online once rather
    # than failing the build for a missing crate nobody has fetched yet.
    if ! cargo run --quiet --offline --manifest-path "$gen_manifest" -- \
        --root "$here" --book "$here/docs" --out "$site" \
        --summary "$here/docs/SUMMARY.md" --stamp "$(stamp)" \
        --corpus "$here/docs" --corpus "$here/spikes" --corpus "$here/README.md"; then
        echo "docs.sh: offline build failed; retrying with network" >&2
        cargo run --quiet --manifest-path "$gen_manifest" -- \
            --root "$here" --book "$here/docs" --out "$site" \
            --summary "$here/docs/SUMMARY.md" --stamp "$(stamp)" \
            --corpus "$here/docs" --corpus "$here/spikes" --corpus "$here/README.md" ||
            die "the generator failed to build/run (see above)"
    fi
    t1=$(date +%s)
    echo "site: $site  ($((t1 - t0))s)"
    echo "open: ${SITE_URL_HINT:-file://}$site/index.html"
}

case "${1:-build}" in
    build)
        shift || true
        build "$@"
        ;;
    clean)
        rm -rf "$site"
        echo "removed ${site#"$here"/}"
        ;;
    serve)
        require python3
        build
        shift || true
        exec python3 "$here/scripts/docs_serve.py" --site "$site" --root "$here" "$@"
        ;;
    check)
        require python3
        exec "$here/scripts/docs_check.sh" "${@:2}"
        ;;
    *)
        cat >&2 <<'USAGE'
usage: ./scripts/docs.sh [build|serve|check|clean]

  build   render docs/ into docs/_site (default)
  serve   build, then serve on http://localhost:3000 with live reload
  check   the docs rot gate (also runs as a step of ./scripts/check.sh)
  clean   remove docs/_site
USAGE
        exit 2
        ;;
esac
