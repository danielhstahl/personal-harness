# ADR-007: The notice moves to ntfy — a URL instead of an address

- **Status:** Accepted. **Supersedes** the transport half of
  [ADR-005](./ADR-005-completion-notice.md) (the mail adapter and everything
  that existed only because email existed). The notice itself — the effect the
  machine asks for, the ordering, the failure containment, the give-up streak —
  stands unchanged.
- **Modules:** `src/ntfy.ts` (the publish transport: target resolution,
  the JSON publish envelope, byte limits, HTTP), `src/notify.ts` (the wording,
  re-shaped for a phone), `src/orchestrator.ts` (`notify.publish`, renamed from
  `notify.email`), `src/loop.ts` (the handler), `src/app.ts` / `src/main.ts`
  (composition and the `LOOP_NTFY_*` knobs)

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
POST https://ntfy.sh/                  (or http://192.168.1.20:8080/)
Content-Type: application/json

{"topic":"loop",
 "title":"[pi-beads] tst.42 completed: Added colour handling to the tokenizer…",
 "priority":4,
 "tags":["+1"],
 "message":"tst.42 — Add the colour mode to the parser\n…"}
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
ntfy, and the publish goes to the prefix, which is the root of the ntfy behind
it.

The topic name itself is checked against ntfy's own rule (`^[-_A-Za-z0-9]{1,64}$`,
and note that a dot is not in it), because a topic outside that rule is a
guaranteed `400` on every single notice, and a guaranteed failure is worth
hearing once, in the terminal the typo was typed in.

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
`limit-message-title-length: 200`. The message is truncated at the byte limit on a
character boundary and marked `… (truncated)`; the title is clipped in a way that
keeps the leading `[prefix] bead.id`. Cutting to fit beats a `413`, and marking
the cut beats a notice that silently reads as shorter than the run.

The JSON body has a second ceiling that the header form did not have to think
about: `transformBodyJSON` reads the document with `MessageSizeLimit*2`, so the
whole envelope — escaping included — has to fit 8 KiB while the message inside it
fits 4 KiB. A message of 4 KiB made of quotes is 8 KiB of JSON before the topic
and the title are added, so the message is clamped twice: to its own limit, then
to whatever the document limit leaves. Both clamps say so.

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

## Amendment: the notice moved out of the headers and into the body

- **Status:** Accepted. Amends §1 and §5 above; the ordering, the containment,
  the give-up streak and the wording all stand exactly as decided.
- **Trigger:** a real run, a real closed bead, and this transcript line:

  ```
  Could not publish the completion notice for workspace-a05: Invalid character
  in header content ["Title"]. The bead is closed either way — nothing about the
  work changed.
  ```

### What happened

`Title:` is a header, and a header is a control-character-free, Latin-1-sort-of
thing. Node refuses anything above U+00FF at the socket layer
(`ERR_INVALID_CHAR`) before a byte of the request leaves the process. The title
of this notice is

```
[pi-beads] <bead id> completed: <the summary the agent reported>
```

and that summary is prose written by a model. An em dash, a curly quote, a `✓`
it typed because the tests pass, a term copied out of a file in another language:
any one of them in a sentence about the work and the notice cannot be sent —
while the very same notice with a plainer summary sails through.

That is a shape of bug worth writing down, because everything about it
misleads:

- **It looks intermittent.** Half the beads announce themselves and half do
  not, and which half is decided by the *vocabulary* used to describe the
  work. Nothing about the run predicts it.
- **It looks like the server's fault.** It arrives in the delivery report,
  alongside "ntfy replied 503" and "connection refused", so the natural
  place to look is the server. No request was ever sent.
- **It cannot be reviewed away.** Nothing in the diff of the ticket that
  triggers it is wrong. The trigger is a word.

The containment decided in §2 did its job — the bead stayed closed, the run
kept going, the failure was legible — which is why this was a notice problem
and not an incident. But a notice that quietly does not fire for some tickets
is a notice nobody can trust, and "we will tell you about the finished work"
does not come with a character-set exclusion list.

### The fix

Say the same thing somewhere that is not a header. ntfy accepts the whole
publish as a JSON request body (its documented "Publish as JSON" form, made
for integrations that cannot set headers at all); we were using the older
form out of habit:

```
POST <server>/              ← the root, not <server>/<topic>
Content-Type: application/json
Authorization: Bearer …     ← the one thing that has to stay a header

{"topic":"loop","message":"…","title":"…","priority":4,"tags":["+1"]}
```

Everything the run wrote — title, message, tags, click URL — is now UTF-8 in a
JSON string, where anything the run produced is legal. What is left in a header
is `Content-Type`, `User-Agent` and the bearer token: three constants and one
operator-supplied secret, none of it agent-authored.

The two formats are equivalent where it matters. ntfy's own `transformBodyJSON`
decodes the envelope and sets the very same `X-Title` / `X-Priority` /
`X-Tags` / `X-Click` headers server-side, then calls the same publish handler.
Nothing was traded away for the robustness: headers were always one way of
spelling that envelope, and the worse-spelled way.

### Four things the JSON form made us check instead of assume

1. **The URL is the root, not the topic.** ntfy routes a JSON publish on
   `r.URL.Path == "/"`. `POST /mytopic` with a JSON body is not the same
   request said differently — that route takes its topic out of the *path*
   and makes the entire JSON document the text of the notification, which
   "delivers" and reads like a stack trace. So `NtfyTarget` now carries
   `destination` (the `host/topic` string a human recognises, still what the
   delivery report names) and `publishUrl` (where the request actually goes)
   as two fields rather than one string pressed into both jobs.
2. **`priority` is an `int`.** `high` is fine in a header and a `400` in a
   JSON body, because `publishMessage.Priority` is an `int` in Go. Names are
   translated on the way in (`min`→1 … `urgent`→5), and a value that is
   neither is refused rather than shipped.
3. **`tags` is an array of strings**, not the comma-joined string the header
   took.
4. **The document has its own ceiling**, 2× the message limit, and escaping
   spends it. Hence the second clamp in §5.

### And one rule it made explicit

Nothing the work wrote may sit in a header. That is enforced rather than
remembered: the one remaining operator-supplied header value, the token, is
checked at startup (`notify-config`, exit 2 — a token with a stray newline is
an operator error, reported where it was typed) and checked again inside the
publisher, where all it can become is a `failed` delivery that names
`LOOP_NTFY_TOKEN` instead of an `ERR_INVALID_CHAR` that names nothing a person
can act on.

## Amendment: a notice is never an attachment — the limit is exclusive

- **Status:** Accepted. Amends §5 above.
- **Trigger:** the next failure of the same notice, after the header problem was
  fixed:

  ```
  Could not publish the completion notice for workspace-mm6: ntfy replied 400:
  {"code":40014,"http":400,"error":"invalid request: attachments not allowed",
   "link":"https://ntfy.sh/docs/config/#attachments"}
  ```

  The reasonable question is the one that was asked: *why are attachments even
  being used?* They are not. Nothing in this feature mentions an attachment. The
  notice was simply the wrong size to be anything else.

### What happened

The decision is made by three lines of ntfy that have never been read together:

```go
// util/peek.go
read, err := io.ReadFull(underlying, peeked)          // peeked is `limit` bytes long
return &PeekedReadCloser{ LimitReached: read == limit, … }

// server/server.go — handlePublishBody
} else if !body.LimitReached && utf8.Valid(body.PeekedBytes) {
    return s.handleBodyAsTextMessage(m, body)          // Case 6: a message
}
return s.handleBodyAsAttachment(r, v, m, body)         // Case 7: a file

// server/server.go — handleBodyAsAttachment
if s.attachment == nil || s.config.BaseURL == "" {
    return errHTTPBadRequestAttachmentsDisallowed      // 40014
}
```

`LimitReached` is `read == limit`. **Exactly at the limit counts as reached.**
A message that lands on `limit-message-bytes` therefore fails the Case 6 guard
and falls to Case 7 — and `attachment-cache-dir` is unset by default, so Case 7
answers `40014 attachments not allowed`. On a server that *does* have an
attachment store it answers `200`, and the notice arrives as a file to be
downloaded rather than a message to be read, which is a worse failure and a
louder one to nobody.

And our own truncation was landing there. The rule was: cut to
`4096 - markerBytes`, then put `… (truncated)` back on the end.

| body | cut to | sent | at the limit? |
| --- | --- | --- | --- |
| multi-line text, 5 000 B | last whole line | 4 064 B | no — lucky |
| one long line, 6 000 B | `4096 − 15` | **4 096 B** | yes — an attachment |

That is the whole bug, and it explains the shape of the report: it only happens
when the text has no line break in the last stretch of the window, which means
it depends on the length of a paragraph. The multi-line notices that worked were
not working because they were safe; they were working because they happened to
cut early.

### The fix

**Treat ntfy's limit as exclusive.** The message is built under
`limit − 1` bytes, so it is always a message. The slack is a named constant with
this reasoning attached to it (`NTFY_MESSAGE_BOUNDARY_SLACK_BYTES`) rather than
a `- 1` somewhere, because the next person to touch a byte budget is going to
ask why the boundary is reserved.

Two things come with it, because "the server's limit" is not knowable at
compile time:

- **`LOOP_NTFY_MAX_MESSAGE_BYTES`** declares what your server runs. ntfy does
  not publish its own number, so on a tightened self-hosted box the default of
  4096 is a guess; this makes it a fact.
- **A learned cap.** If a refusal means "too much message" — `40014`, `413`,
  or the "too large" wording — the publisher halves the cap, keeps it for the
  rest of the run, and logs the act. Against a 1 KiB server that is three
  requests on the first notice (`4095 → 2047 → 1023`) and one on every notice
  after. It stops in three places rather than grinding: the floor of a useful
  notice, the run's shrink budget, and — the one that matters most — the
  notice's own size. A 100-byte notice under a 4 KiB cap sends the same bytes
  under a 1 KiB cap, so once the next cap is not smaller than what is already
  being sent there is no "less" left to try and the refusal is about something
  else.

The last of those is why a `413` from a proxy is not retried into a hole:
shrinking a notice that is already small is not a retry, it is the same
request sent twice.

### What is still true

A notice bigger than the server's limit on a server **with** attachments
enabled will still be filed as a file — the server accepted it, so there is
nothing for the client to react to. That is what the knob is for: if you run an
attachment store and do not want your completion notices in it, set
`LOOP_NTFY_MAX_MESSAGE_BYTES` to the limit you configured, and the loop will
never produce one that long.
