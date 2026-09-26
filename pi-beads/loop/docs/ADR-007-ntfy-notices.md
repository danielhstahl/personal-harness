# ADR-007: The notice moves to ntfy — a URL instead of an address

- **Status:** Accepted. **Supersedes** the transport half of
  [ADR-005](./ADR-005-completion-notice.md) (the mail adapter and everything
  that existed only because email existed). The notice itself — the effect the
  machine asks for, the ordering, the failure containment, the give-up streak —
  stands unchanged.
- **Modules:** `src/ntfy.ts` (the publish transport: target resolution,
  headers, byte limits, HTTP), `src/notify.ts` (the wording, re-shaped for a
  phone), `src/orchestrator.ts` (`notify.publish`, renamed from `notify.email`),
  `src/loop.ts` (the handler), `src/app.ts` / `src/main.ts` (composition and
  the `LOOP_NTFY_*` knobs)

## Context

ADR-005 added an email notice per closed bead, and it worked. It also cost what
email costs:

- **1 319 lines of mail adapter** for an RFC 822 renderer, quoted-printable,
  folded headers, encoded words, and a hand-rolled SMTP client with STARTTLS,
  `AUTH PLAIN`/`LOGIN`, capability negotiation and redaction — plus a fake SMTP
  relay in the test suite to exercise it.
- **A configuration surface with twelve variables** in it, half of which existed
  to answer questions about transport security that only a mail relay asks: is
  STARTTLS required or optional, may a password cross an unencrypted channel,
  does the `From` header match the authenticated sender.
- **An identity requirement that is not the loop's business.** Mail can't be
  sent without a sender address that a relay will accept, which drags in SPF,
  DKIM and "why does Gmail think this is spam" — none of which have anything to
  do with telling a human that a bead closed.

The actual requirement was never "email". It was: **something finished, I'm not
looking at the terminal, tell me.** ntfy is that, with a topic URL:

```
POST https://ntfy.sh/<topic>          (or http://192.168.1.20:8080/<topic>)
Title: [pi-beads] tst.42 completed: Added colour handling to the tokenizer…
Priority: high

tst.42 — Add the colour mode to the parser
…
```

No handshake, no session, no sender identity, no relay policy — and the same
variable that names the hosted server names a box on the LAN. **Local hosting is
one value, not one configuration**, which is the property the mail version could
not have.

## Decision

### 1. The topic replaces the address, and the URL is the whole story

`LOOP_NTFY_TOPIC` is the switch, exactly as `LOOP_NOTIFY_EMAIL` was: unset means
no publisher, no socket, and a transcript that says nobody was told.

It takes either a bare topic name (`loop-notices`) or a complete URL
(`https://ntfy.sh/abc123-notify`), because the second form is what the ntfy web
UI hands you and the first is what you'd write in a dotenv file. A bare topic is
joined onto `LOOP_NTFY_URL`, which defaults to `https://ntfy.sh`. A base with a
path prefix (`https://example.test/ntfy`) is kept — that's a reverse-proxied
ntfy, and the topic lives under the prefix.

`LOOP_NTFY_URL` alone is *not* the switch. A server with nowhere to publish is a
relay that will never be exercised, and `enabled: false` with a reason beats a
silent no-op.

### 2. Bad config is refused at build time; bad delivery never stops a run

Unchanged from ADR-005, because that rule was right: a URL with no scheme, a URL
with credentials stuffed into it, a topic that resolves to nothing and a
priority that is neither 1–5 nor one of ntfy's five names all stop the loop at
startup with `notify-config` and exit 2. A publish that fails costs a warning and
a transcript line.

The credential rule is worth one sentence: ntfy authenticates with a bearer
header, so a `user:pass@` in the URL is a credential with nowhere legitimate to
go — and a URL is a string that gets echoed into logs, error messages and
delivery reports by everything that touches it. Refusing it is cheaper than
tracking where that string ended up.

### 3. Delivery is data: `delivered` / `skipped` / `failed{retryable}`

Same shape as the mail transport's, because the shape was the good part. The
retry rule is ntfy-specific and comes from ntfy's own status codes:

| Status | Means | Retry? |
| --- | --- | --- |
| `2xx` | queued and pushed | — |
| `429` | rate limited | **yes** |
| `400`/`403`/`404` | bad request, wrong topic, wrong token | **no** |
| `5xx` | the server had a problem | **yes** |
| timeout / connection refused | nothing answered | **yes** |

A `403` retried is a pile of identical refusals; a `429` not retried is a lost
notice over a hiccup.

### 4. The message is shaped for a phone, not a desk

This is the part that changed more than the transport. An email is read at a desk
and can be a page; a notification is read in a glance, on a lock screen, often
while standing. So the notice is four things worth acting on plus a pointer, not
nine sections:

```
[pi-beads] tst.42 completed: Added colour handling to the tokenizer and thread it through the parser.

tst.42 — Add the colour mode to the parser
Added colour handling to the tokenizer and thread it through the parser.

done · 3m 11s · iteration 3 · 2025-10-26 11:03 UTC
deadbeefcafe · 2 files
- src/colour.ts
- src/parser.ts

next: docs still need the colour section
read: bd show tst.42 · bd recall loop:handoff:tst.42
from /work/project on buildhost
```

The hash is shortened (long enough to paste, short enough to read), the file list
folds past six, the next-step list says `(+2 more)` rather than running off the
screen, and everything the run could not know still says so rather than vanishing.
The **title keeps the bead id at the front** for the same reason it always did: a
notification is found by its title long after it arrives.

### 5. ntfy's limits are honoured, not discovered

ntfy's defaults are `limit-message-bytes: 4096` and
`limit-message-title-length: 200`. The body is truncated at the byte limit on a
character boundary and marked `… (truncated)`; the title is clipped in a way that
keeps the leading `[prefix] bead.id`. Cutting to fit beats a `413`, and marking
the cut beats a notice that silently reads as shorter than the run.

### 6. The give-up streak carries over, unchanged

Three consecutive failures switch notices off for the rest of the run, a success
resets the counter, and the switch-off reason travels on the delivery that caused
it. `Notifier.enabled` is a getter for the same reason it was made one in the
mail version — after the guard trips, "is it on?" must answer no.

What the streak guards against is the same either way: a loop that publishes on
every closed bead against a server that is down would otherwise discover that
once a bead for six hours.

## Consequences

**The knobs.** `LOOP_NTFY_TOPIC` (the switch; bare name or full URL),
`LOOP_NTFY_URL` (the server, default `https://ntfy.sh`), `LOOP_NTFY_TOKEN`
(bearer), `LOOP_NTFY_PRIORITY` (1–5 or `min`/`low`/`default`/`high`/`urgent`),
`LOOP_NTFY_TAGS` (emoji names), `LOOP_NTFY_CLICK` (URL the notification opens),
`LOOP_NTFY_TITLE_PREFIX` (default `pi-beads`), `LOOP_NTFY_TIMEOUT_MS`
(default 10s), `LOOP_NTFY_MAX_FAILURES` (default 3). In the container they are
ordinary `-e` flags.

**Fewer knobs, and no security-shaped ones.** The mail version had twelve
variables and half of them were about transport security. ntfy either speaks TLS
because you pointed it at a `https://` URL or it doesn't, and there is no
password to leak onto an unencrypted channel beyond the bearer token, which is
only ever a header value and never appears in a log line (there's a test that
publishes with a token and asserts the string appears nowhere in the log).

**Self-hosting is a first-class case, not an escape hatch.**
`LOOP_NTFY_URL=http://192.168.1.20:8080` is the whole story. Two things to
know: for a self-signed certificate, point Node's TLS trust at your CA with
`NODE_EXTRA_CA_CERTS` rather than turning verification off — this module has no
"skip TLS verification" setting and adding one should be its own decision. And a
reverse-proxied server under a path prefix works: the prefix is preserved.

**Local delivery is a subscription away.** ntfy's CLI can subscribe
(`ntfy subscribe <topic>`) and the phone app is one tap; nothing in this loop
needs to know which. Compare email, where the loop had to know a mail server's
whole personality.

**What mail had that this doesn't.** Threaded replies (a notice can't be replied
to, so the body carries `bd show` and `bd recall` instead of a reply thread),
guaranteed delivery (ntfy is best-effort push with a cache window; mail is a
store-and-forward system), and a persistent archive in an inbox. The trade was
made deliberately: the notice is an *index into the real record*, and the record
is the beads database and git, not the notification channel. Losing reply threads
costs nothing when the body already says `bd show tst.42`.

**The migration is a rename with no migration path.** `LOOP_NOTIFY_EMAIL`,
`LOOP_NOTIFY_CC`, `LOOP_NOTIFY_FROM`, `LOOP_NOTIFY_SUBJECT_PREFIX` and the
`LOOP_MAIL_*` family are gone, as is `src/mail.ts`. `notify.email` is
`notify.publish`. Nothing converts the old variables — a stale
`LOOP_NOTIFY_EMAIL` in a deployed environment silently doing nothing would be
exactly the kind of ghost this codebase keeps having to exorcise, so the answer
is that unset is unset and the transcript says so.
