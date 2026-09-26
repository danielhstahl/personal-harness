# ADR-005: The completion notice — a report the run cannot be punished for

- **Status:** Accepted in part. **The transport half is superseded by
  [ADR-007](./ADR-007-ntfy-notices.md)**: the mail adapter and every
  `LOOP_MAIL_*` / `LOOP_NOTIFY_EMAIL` knob are gone, replaced by ntfy and a
  topic URL. What still stands from this document is the notice itself — the
  effect the machine asks for, the ordering after the close, the containment of
  delivery failure, the give-up streak — and the reasoning below for those is
  unchanged. Where this document talks about *mail*, read *the transport of the
  day*; the shape of the argument is what was kept.
- **Modules:** ~~`src/mail.ts`~~ (RFC 822 rendering, the SMTP client, the
  mailer — **removed by ADR-007**; `src/ntfy.ts` is its successor),
  `src/notify.ts` (what a finished bead says, and when to stop trying to say it),
  `src/orchestrator.ts` (the notice effect, now `notify.publish`), `src/loop.ts`
  (the handler and the enrichment), `src/app.ts` (composition, validation at
  build time), `src/main.ts` (env knobs)

## Context

The loop finishes a bead and the fact goes into a database nobody is looking at.
That is fine while the terminal is up and the transcript is scrolling. It is not
fine an hour later, from a phone, when the question is "did the thing I asked for
at 2am actually land, and where is the commit?"

The requirement: **if an email address is supplied in the environment, send one
notice per bead completion, with the bead id in the subject and the relevant
details in the body.**

Six constraints made this more than a `sendmail` call.

**1. A side effect the machine causes has to leave as data.** ADR-001 and the
loop's own design say the orchestrator decides and never *does*; the interpreter
executes. The recipient address lives in the environment, and the environment
lives in the interpreter. A transition that read a config value would stop being
a function of state and event and would stop being testable without a board, a
relay and a mailbox. So the machine needs a way to *ask* for the notice without
knowing whether anybody wants one.

**2. Mail must never be able to stop the work.** By the time a completion notice
is due, the bead is committed, handed off and closed. A relay that is down, a
firewall that eats port 25, a typo in an address — none of those are facts about
the work. A loop that failed a completed iteration because a *report about it*
could not be filed would be punishing the work for the mail's problems, which is
a strictly worse failure than the one being reported.

**3. The dependency list is three packages and all of them are pi.** A mail
library is the kind of dependency nobody re-reads. The surface actually needed is
`EHLO`, `STARTTLS`, `AUTH`, `MAIL FROM`, `RCPT TO`, `DATA`, `QUIT` and a reply
parser. Trading a supply-chain entry for about two hundred lines that can be
tested against a fake relay on loopback — where the whole conversation is asserted
line by line — is the better trade for a notifier, and a worse one for a mail app.
This is a notifier.

**4. A notice must not be able to claim completion before the completion.** The
finalize ritual orders its writes by how much they would lie if they were the
last thing that happened. A mail saying "tst.42 is done" that goes out before the
`bd close` lands is exactly that kind of lie, and it is the one this loop is
built not to tell.

**5. The subject and body are made of agent-written text.** The summary in the
subject line is a sentence a model produced. Unvalidated, a summary containing
`\r\nRCPT TO:<attacker@evil.test>` is not a formatting quirk — it is header
injection into a message the loop's own credentials are authenticating.

**6. Silence and failure must be distinguishable.** "Nobody asked for mail",
"mail is misconfigured", "the relay is down" and "mail is off for this run" are
four different answers, and an operator hunting for a notice that did not arrive
needs to be able to tell them apart from the log.

## Decision

**Add an opt-in completion notice: the machine emits a `notify.email` effect when
a bead closes; `src/notify.ts` words it; `src/mail.ts` renders it and speaks
SMTP; the interpreter reports the outcome and the run continues whatever it was.**

### 1. The effect is asked for by the machine, from the facts it can see

`closed` in `finalize` emits one `notify.email` carrying `issueId`, `title`,
`closeReason`, `commit`, `iteration` and `handoffKey` — every one of them a fact
the transition holds, none of them a value looked up. It goes out **after** the
close and **before** `drop_context`, so a notice can never outrun the write it
reports.

The interpreter enriches it with the facts only the interpreter has, because the
unit that ran produced them: the paths the finalizer *actually* committed (which
beat the verdict's claim list — "what landed" over "what was believed touched"),
the next steps and decisions from the finalize request, the verdict kind, the
elapsed time, the clock. That split is why the effect is not simply the rendered
message: the machine cannot see most of what belongs in it.

Nothing in the loop branches on whether mail is configured. An unwired notifier
records the effect as `skipped(no notifier wired)` and the transcript shows a
notice that had nowhere to go, rather than nothing at all.

### 2. Two modules: what it says, and how it goes

`src/notify.ts` owns the *wording*: the subject line, the body's sections, the
machine-readable headers. `src/mail.ts` owns the *bytes and the protocol*: RFC
822 rendering, quoted-printable, folded headers and encoded-words, the SMTP
client, the mailer. Neither knows the other's details beyond one interface, in
the same relation as `finalize.ts` to `vcs.ts`.

Rendering is deterministic given the clock and the uid — the same input produces
byte-identical output — because "what went out" has to be assertable rather than
inferred. Quoted-printable keeps a plain-English body readable in the raw
message, which matters when a delivery is being debugged from a mail log; spaces
stay literal and the one thing that never survives is a trailing raw space, which
a server may quietly trim.

`src/mail.ts` is the only module in the app that opens a socket to a mail server.
It spawns nothing, so the spawn allowlist in `test/beads.test.ts` is untouched.

### 3. A delivery is data, and every failure path is one

```
delivered  { messageId, transport, recipients }
skipped    { reason }
failed     { reason, transport, retryable }
```

`Mailer.send` does not throw: validation, rendering and the transport call all
sit inside the same catch. The loop's handler maps those three onto `say`,
`log` and `warn`, and returns no effects either way. A notifier that throws
outright is caught too — a badly built notifier is not allowed to be fatal.

A mail that failed is a warning naming the bead, not a stop, and it says so:
*"The bead is closed either way — nothing about the work changed."*

### 4. Give up after three, because a dead relay is discovered once

`notifyCompletion` counts consecutive failures and, at three (configurable via
`LOOP_NOTIFY_MAX_FAILURES`), switches the notifier off for the rest of the run.
One notice per bead is one warning per bead; over a night's run that is a wall of
identical text that hides everything else, and it discovers the same dead relay
forty times. A success resets the counter, so a flaky relay that recovers keeps
working and a genuinely dead one goes quiet after three beads instead of three
hundred.

Two things about the giving-up are load-bearing, and easy to get wrong:

**The switch-off travels on the delivery that caused it.** The third failure's
`reason` carries the whole sentence — *"N completion notices in a row failed to
send (last: …); mail is switched off for the rest of this run"* — because the
delivery is the channel the loop already reports. A notifier-side `logger` seam
exists for tests, but nothing wires it at composition time, so a reason that only
reached the logger would be heard on the *next* bead's turn — and a run that
closes exactly three beads and then goes idle would never hear it at all.

**`Notifier.enabled` is a getter, not a snapshot.** Computed once at construction
it would keep answering `true` for the rest of a run in which nothing will ever
be sent again, which is the precise opposite of what the property is for. The
third failure makes the answer `no`, and anybody asking afterwards — a surface, a
test, the next caller — gets the truth.

### 5. Config the operator typed is refused at build time

An unusable recipient address, a mail URL that will not parse, a password with
no user: these stop the run at startup with `notify-config` and exit 2, the same
way an unknown thinking level does. The alternative is discovering a typo at the
first closed bead and every bead after it — a day of missing mail, discovered by
the person who was waiting for it.

A *delivery* failure is never a startup failure. The line is "can this run have
been asked to send mail at all", not "will this message arrive".

`LOOP_NOTIFY_MAX_FAILURES` is refused here too, and it is worth saying why that
needs saying: the environment reader's `number()` helper returns `undefined` for
anything it cannot parse, so `LOOP_NOTIFY_MAX_FAILURES="nine"` would have fallen
back to the default of three and looked exactly like a setting that took. A knob
that is set has to mean what it says, so a value which is not a safely
representable whole number of at least one fails with `notify-config`. The one
exception is whitespace, which reads as unset — a blank line in a `.env` file is
nobody's opinion about retry counts.

### 6. Nothing unvalidated reaches the wire

- An address is parsed to its `addr-spec` before it goes into `RCPT TO`, and a
  CR or LF in an address is refused outright.
- Header values are flattened: line breaks become spaces, so the subject keeps
  the agent's text and loses the ability to create a header.
- Reserved headers cannot be shadowed by a caller, which would put two
  `Message-ID`s on one message.
- `AUTH` lines are redacted in the client log, by construction: the session logs
  the string it is handed, and only the auth commands are built with redaction.
- A password over a channel that is not encrypted is refused unless
  `LOOP_MAIL_INSECURE_AUTH` says the operator means it.
- `Auto-Submitted: auto-generated` (RFC 3834), so a vacation responder does not
  start a conversation with a build server.

### 7. Off unless asked, and silent about being off

`LOOP_NOTIFY_EMAIL` unset means no notifier, no socket, no poller. The knob is
the address, not a flag next to the address: an address with nothing behind it
and no address at all both end up "nobody is told", and the transcript says which.

A dry run sends nothing — a dry run never reaches the close that triggers the
notice, which is the same reason it never closes a bead.

### 8. A copy is a copy, not a blind copy

`LOOP_NOTIFY_CC` was wired, delivered mail, and was still wrong: the notifier
passed the combined addressee-and-copy list to the mailer as `to`, so the copies
arrived with the primary addressees named in the `To:` header and no `Cc:` header
written anywhere. Functionally that is a blind copy with a header claiming
everybody was a primary — which breaks reply-all expectations, hides who was
merely told, and contradicts the header the rest of the thread is built on.

The fix is to keep the two lists separate all the way down:

- `Mailer` exposes `to` and `cc` alongside the combined `recipients`, and
  `Notifier` passes them as what they are.
- The **envelope** is addressees plus copies, because a message not listed in
  `RCPT TO` simply never arrives; it is deduplicated on the parsed `addr-spec`,
  so somebody written into both fields gets one notice, not two.
- The **headers** keep the distinction: `To:` names the addressees, `Cc:` names
  the copies, and an address already in `To:` is dropped from `Cc:` rather than
  repeated.
- The footer says it in words too — *"to dev@…, cc lead@…"* — because the
  header alone is not something a reader of a plain-text notice looks at.

An explicitly empty `to` is still a skip, not a fallback. `to` is a required
field on a message, so an empty one is a statement that the message is addressed
to nobody; quietly substituting the mailer's configured list would deliver
something the caller did not ask to send. `cc` is optional, so an absent one is
no opinion, and the configured list applies.

## Consequences

**The knobs.** `LOOP_NOTIFY_EMAIL` (recipients, comma/semicolon/space
separated — the switch), `LOOP_NOTIFY_CC`, `LOOP_NOTIFY_FROM`,
`LOOP_NOTIFY_SUBJECT_PREFIX`, `LOOP_MAIL_URL`
(`smtp://user:pass@host:587`, `smtps://host:465`), `LOOP_MAIL_HOST`,
`LOOP_MAIL_PORT`, `LOOP_MAIL_USER`, `LOOP_MAIL_PASSWORD`,
`LOOP_MAIL_STARTTLS` (`required` / `optional` / `off`),
`LOOP_MAIL_TIMEOUT_MS`, `LOOP_MAIL_INSECURE_AUTH`,
`LOOP_NOTIFY_MAX_FAILURES` (the give-up threshold, default 3). In the container
they are ordinary `-e` flags; nothing in the image needs rebuilding to turn mail
on.

**What mail can actually deliver is the relay's problem.** This sends mail; it
does not sign it. SPF, DKIM, DMARC, reverse DNS and whether a provider accepts a
sender at all belong to the host and the relay, and are the reason
`LOOP_NOTIFY_FROM` exists as a first-class knob rather than a guess at a hostname.

**Not built, on purpose.** Notices for failed beads — the failure is already on
the board under the failure key, and the loop already says it out loud. Digests
and batching — "one mail per bead" is what was asked for, and a batch is a
different feature with a different trigger. A template system — the notice is
plain text assembled from the record, and the record is where the truth lives.

**Where it is tested.** `test/mail.test.ts` drives the real SMTP client against a
fake relay on loopback and asserts the conversation, the redaction, the refusal
paths and the timeout. `test/notify.test.ts` asserts the wording, the headers and
the failure streak. `test/loop.test.ts` asserts that a closed bead produces one
enriched notice between the close and the cold boundary, that a failed delivery
warns without changing the run, that a throwing notifier is contained, and that
"no notifier wired" is a reported state rather than a dropped effect.
