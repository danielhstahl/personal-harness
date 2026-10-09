//! Out-of-band notification: a fact for a human who is *not looking at this terminal*.
//!
//! The beads loop buys real passes with real money and can spend a long stretch doing
//! it, which is exactly long enough to walk away. This module is the way out of the
//! terminal: the loop hands over a finished ticket and something else deals with
//! telling a person.
//!
//! ## Why a channel, and why the sink arrives through `SessionConfig`
//!
//! The producer is the beads session's own task — the task that drives the pass
//! machine and services `Esc` and `Shutdown`. **It must never await the network.**
//! A ntfy endpoint that hangs would hang cancellation with it, and a wedged task on
//! the wrong side of that boundary is far worse than one lost doorbell. `notify` is
//! therefore a `send` into a queue whose reader is a separate, long-lived task
//! ([`Ntfy::spawn`]), and the sink travels as an `Arc<dyn Notifier>` on
//! [`SessionConfig`](crate::session::SessionConfig) — the same injection route the
//! fake binaries take, so nothing in `spawn` / `default_factory` / the Router
//! factory gained a parameter to carry it.
//!
//! The trait rather than a concrete type keeps the *destination* out of the loop's
//! hands, which leaves three sinks and no way for the loop to pick one by accident:
//!
//! * production: [`Ntfy`], built by [`notifier_from_env`] from
//!   `LOOPRS_NTFY_URL` / `LOOPRS_NTFY_TOPIC`;
//! * [`Noop`], which is what [`SessionConfig::default`]`()` carries — that is what
//!   keeps the whole test suite network-free **by construction** rather than by
//!   nobody remembering to unset an env var;
//! * [`crate::testing::RecordingNotifier`], which remembers instead of posting, so
//!   a test can assert the completion edge itself.
//!
//! ## What fires, and what must not
//!
//! **A ticket the board confirms closed, and nothing else.** `agent_settled` is not
//! completion: it is the worker having stopped talking, and the settle that follows
//! an abort, a *planner's* settle, and a pass that left the ticket `open` all look
//! identical on the wire. The only completion fact this harness has is
//! `PassOutcome::Closed` (in `session::beads`), which is the board's verdict rather
//! than the agent's — which is why exactly one [`Notifier::notify`] call exists in
//! the whole binary, in that arm of `BeadsTask::worker_settled`, and none anywhere
//! near the settle itself.
//!
//! A ticket a worker left `blocked` is deliberately **not** announced
//! (`PassOutcome::LeftForHuman`). The loop did not finish that one, and the harness
//! that cannot close a ticket has no business sending an all-clear about it; if the
//! alert wants a second trigger later, that is a new decision with its own text,
//! not this arm widened.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

/// How long one ntfy POST may take before it counts as failed.
const POST_TIMEOUT: Duration = Duration::from_secs(10);

/// The gap before the single retry, and how many tries in total.
///
/// One retry, always, and then give up and say so loudly. What this covers is the
/// case that actually happens: a notification sent hours after anybody was watching,
/// over a link that blipped. What it deliberately does *not* try to be is a queue of
/// record — `bd` is the record — so a second failure is an error line, not a loop
/// that never stops trying against an endpoint nobody configured correctly.
const POST_ATTEMPTS: u32 = 2;
const RETRY_AFTER: Duration = Duration::from_secs(2);

/// The fact worth interrupting somebody for: a ticket this harness worked, and the
/// board confirmed closed.
///
/// Two fields and no more. The notifier gets the id so the receiver can find the
/// ticket, and the title so they do not have to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BeadDone {
    pub id: String,
    pub title: String,
}

impl BeadDone {
    /// The one-line form, used as the notification body.
    ///
    /// Falls back to the bare id when the title is blank: `"looprs-1 — "` is worse
    /// than `"looprs-1"`.
    pub fn summary(&self) -> String {
        let title = self.title.trim();
        if title.is_empty() {
            self.id.clone()
        } else {
            format!("{} — {}", self.id, title)
        }
    }
}

/// A sink for completion facts.
///
/// **Implementations may not block, and may not fail the caller.** `notify` returns
/// `()` rather than a `Result` on purpose: making the beads loop handle "the
/// notification did not work" would mean either a panicking money-spending loop or
/// one that parks on a doorbell, and neither is what anybody asked for. Everything
/// that *can* fail — DNS, TLS, a 500, a closed queue — happens in the sink's own
/// task and is logged there.
pub trait Notifier: Send + Sync + std::fmt::Debug + 'static {
    fn notify(&self, done: BeadDone);
}

/// The sink that throws the fact away.
///
/// The default everywhere, and the fallback when the notifier is unconfigured or
/// unbuildable. Silent by design, but not silently: the *decision* to use it is
/// logged by [`notifier_from_env`], so "why did nothing arrive on my phone?" has an
/// answer in the log.
#[derive(Clone, Copy, Debug, Default)]
pub struct Noop;

impl Notifier for Noop {
    fn notify(&self, _done: BeadDone) {}
}

/// The [ntfy](https://ntfy.sh) sink: one message per completed ticket, on a topic.
///
/// Constructed by [`Ntfy::spawn`], which keeps the receiving half of the queue and
/// the task that reads it. A `Ntfy` value is only the **sending** half — cheap to
/// clone, `Debug`, and safe to hold in a config that gets handed around — which is
/// the whole point of the split: dropping the last sender is what ends the poster
/// task, so the task's lifetime is exactly the notifier's.
#[derive(Debug)]
pub struct Ntfy {
    tx: mpsc::UnboundedSender<BeadDone>,
    /// Retained only so the log can name the topic a post would not reach.
    topic: String,
}

impl Ntfy {
    /// Build the queue and spawn the task that drains it to `base_url`/`topic`.
    ///
    /// The queue is unbounded deliberately: a bounded one reintroduces "the loop
    /// waits on the notifier" through the back door, and the backlog it would be
    /// protecting against is one small struct per completed ticket.
    pub fn spawn(base_url: &str, topic: &str) -> anyhow::Result<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<BeadDone>();
        // One client for the whole task: connection reuse across tickets, and a TLS
        // build failure is a startup error rather than a surprise mid-pass.
        let client = reqwest::Client::builder().timeout(POST_TIMEOUT).build()?;
        let url = ntfy_url(base_url, topic);
        // The task's own copy: the sink keeps a second one, so the closure's move
        // and the struct's field are never fighting over one String.
        let task_topic = topic.to_string();

        tokio::spawn(async move {
            // Ends when every sender is gone, which is the notifier's own lifetime.
            while let Some(done) = rx.recv().await {
                post(&client, &url, &done).await;
            }
            tracing::debug!("ntfy sender for `{}` finished: no senders left", task_topic);
        });

        Ok(Self {
            tx,
            topic: topic.to_string(),
        })
    }
}

/// The full post target: `<base>/<topic>`, however the base was spelled.
///
/// A trim rather than a URL parser because the doubled slash is the mistake this
/// prevents, and a hand-typed `LOOPRS_NTFY_URL=https://ntfy.sh/` is full of it.
fn ntfy_url(base: &str, topic: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), topic)
}

impl Notifier for Ntfy {
    fn notify(&self, done: BeadDone) {
        // A failed send means the poster task is gone. Named and dropped: the
        // ticket is still closed, and this is not the loop's problem.
        if let Err(e) = self.tx.send(done) {
            tracing::warn!(
                "notification not queued: the ntfy sender for `{}` is gone ({e})",
                self.topic
            );
        }
    }
}

/// The notifier the environment asks for: ntfy when the pair is complete, silence
/// otherwise.
///
/// Silence is the default and not an error. This is an optional affordance on a
/// personal harness, and "no topic configured" must not read as "refuse to start".
/// A *configured but unbuildable* notifier degrades to [`Noop`] with the failure
/// logged rather than killing startup for the same reason: the beads loop is the
/// feature, and it works without the doorbell.
pub fn notifier_from_env() -> Arc<dyn Notifier> {
    let cfg = ntfy_settings(env_value("LOOPRS_NTFY_URL"), env_value("LOOPRS_NTFY_TOPIC"));
    let Some((url, topic)) = cfg else {
        // `info`, not `debug`: this is a startup resolution, and the operator
        // page's grep index asks for it by name (`grep 'notifications: off'`).
        // Every other knob's resolved-once line is already `info`; a run that
        // quietly downgraded the *off* half of that set to `debug` made the grep
        // answer "nothing", which reads as "the app never checked" rather than
        // "you set neither variable" (looprs-00u.13).
        tracing::info!("notifications: off (set LOOPRS_NTFY_URL and LOOPRS_NTFY_TOPIC to turn on)");
        return Arc::new(Noop);
    };
    match Ntfy::spawn(&url, &topic) {
        Ok(n) => {
            // The whole target, topic included: when nothing arrives on the phone,
            // "where did it go?" is the first question, and the log is local (and
            // gitignored) so it is the right place to answer it.
            tracing::info!("notifications: ntfy → {url}");
            Arc::new(n)
        }
        Err(e) => {
            tracing::error!("notifications disabled: cannot build the ntfy sender: {e:#}");
            Arc::new(Noop)
        }
    }
}

/// The ntfy pair, or `None` unless **both** are present and non-blank.
///
/// Pure, and taking the two values as arguments rather than reading the process
/// environment itself, is the point: the rule — a half-configured notifier is *no*
/// notifier, and whitespace is not a value — is then testable as a table instead of
/// as a mutation of global state that every other test in the process shares.
fn ntfy_settings(url: Option<String>, topic: Option<String>) -> Option<(String, String)> {
    let url = url.map(|v| v.trim().to_string()).unwrap_or_default();
    let topic = topic.map(|v| v.trim().to_string()).unwrap_or_default();
    if url.is_empty() || topic.is_empty() {
        None
    } else {
        Some((url, topic))
    }
}

/// An env var as a value, with empty treated as unset.
fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// One POST per completion, retried once. Split out from the sink so the queue's
/// reader is a four-line loop and the failure ladder is readable on its own.
async fn post(client: &reqwest::Client, url: &str, done: &BeadDone) {
    let mut last = String::from("no attempt made");
    for attempt in 1..=POST_ATTEMPTS {
        match try_post(client, url, done).await {
            Ok(()) => {
                tracing::info!("notified: {}", done.summary());
                return;
            }
            Err(e) => {
                last = e.to_string();
                // Only the last attempt stays quiet; in between, say what is coming
                // so a two-second gap in the log is not a mystery.
                if attempt < POST_ATTEMPTS {
                    tracing::warn!("ntfy post failed ({last}); retrying in {RETRY_AFTER:?}");
                    tokio::time::sleep(RETRY_AFTER).await;
                }
            }
        }
    }
    tracing::error!("notification for {} never reached ntfy: {last}", done.id);
}

/// The single attempt, with ntfy's own reason attached to a non-2xx.
async fn try_post(client: &reqwest::Client, url: &str, done: &BeadDone) -> anyhow::Result<()> {
    let resp = client
        .post(url)
        // ntfy reads its message from the request *body* and its chrome from
        // headers. Posting JSON (as an earlier draft of this did) does not set the
        // title: it puts the literal JSON on the phone.
        //
        // The id only, and only after `ascii_header`: the `http` crate rejects a
        // header value holding a non-visible-ASCII byte, and ticket titles are full
        // of them. The title belongs in the body, which is UTF-8.
        .header("Title", format!("looprs: {}", ascii_header(&done.id)))
        // 3 = default priority on ntfy's 1..=5 scale; a finished ticket is a
        // "look when you like", not a wake-up call.
        .header("Priority", "3")
        .header("Tags", "white_check_mark")
        .body(done.summary())
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    // `Ok(200)`-style success is the only thing that counts as delivered; the body
    // carries ntfy's own reason, and a bare "403 Forbidden" is not actionable.
    let why = resp.text().await.unwrap_or_default();
    anyhow::bail!("ntfy replied {status}{}", brief(&why, 200))
}

/// Shorten to `max` chars for a log line, on char boundaries (never mid-emoji).
fn brief(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max).collect::<String>())
}

/// Force a string into something an HTTP header value may legally hold.
///
/// The `http` crate rejects header values containing a character outside visible
/// ASCII, and a ticket title — or any id a human typed — is full of ones that live
/// outside it: an em dash, an accented letter, an emoji. Rather than fail the post,
/// the header gets a lossy ASCII rendering. The full UTF-8 title goes in the body,
/// which is where ntfy reads a message from anyway.
fn ascii_header(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_completed_bead_is_summarised_with_its_title_and_falls_back_to_its_id() {
        let full = BeadDone {
            id: "looprs-1".into(),
            title: "the notification fires".into(),
        };
        assert_eq!(full.summary(), "looprs-1 — the notification fires");

        let blank = BeadDone {
            id: "looprs-2".into(),
            title: "   ".into(),
        };
        assert_eq!(
            blank.summary(),
            "looprs-2",
            "a blank title leaves no dangling dash"
        );
    }

    /// The whole configuration rule, as a table: **both or neither**.
    ///
    /// A half-set notifier is the misconfiguration that would otherwise produce a
    /// silent no-op — or, worse, a POST to `https://ntfy.sh/` with no topic — so
    /// it is the row this test is really about.
    #[test]
    fn the_notifier_needs_a_url_and_a_topic_and_is_off_for_lesser_pairs() {
        let both = Some("https://ntfy.sh".to_string());
        let and = Some("my-topic".to_string());
        assert_eq!(
            ntfy_settings(both.clone(), and.clone()),
            Some(("https://ntfy.sh".into(), "my-topic".into()))
        );

        let cases: [(&str, Option<String>, Option<String>); 5] = [
            ("no url at all", None, and.clone()),
            ("no topic at all", both.clone(), None),
            ("neither", None, None),
            ("a blank url", Some("   ".into()), and.clone()),
            ("a blank topic", both.clone(), Some("".into())),
        ];
        for (name, url, topic) in cases {
            assert_eq!(
                ntfy_settings(url, topic),
                None,
                "{name} must not produce a notifier"
            );
        }
    }

    /// `Noop` accepts the fact and is unbothered, and — the load-bearing half —
    /// `Noop` is what a default config carries, so no test can reach the network
    /// by forgetting to set something.
    #[tokio::test]
    async fn the_default_sink_hears_nothing_and_never_uploads_it() {
        let n: Arc<dyn Notifier> = Arc::new(Noop);
        n.notify(BeadDone {
            id: "looprs-3".into(),
            title: "quiet".into(),
        });
        // No panic, no block, no socket. The config half is asserted in
        // `the_default_config_carries_the_silent_sink` (session layer).
    }

    #[test]
    fn the_post_target_never_doubles_the_slash_it_was_handed() {
        assert_eq!(
            ntfy_url("https://ntfy.sh", "looprs"),
            "https://ntfy.sh/looprs"
        );
        assert_eq!(
            ntfy_url("https://ntfy.sh/", "looprs"),
            "https://ntfy.sh/looprs",
            "a trailing slash in the env var is the common case, not the odd one"
        );
        assert_eq!(
            ntfy_url("http://localhost:8080///", "looprs"),
            "http://localhost:8080/looprs"
        );
    }

    #[test]
    fn a_long_ntfy_reason_is_shortened_without_breaking_a_multibyte_char() {
        let long = "é".repeat(400);
        let clipped = brief(&long, 10);
        assert_eq!(clipped.chars().count(), 11, "10 kept + the ellipsis");
        assert!(clipped.ends_with('…'));
        assert_eq!(brief("  short  ", 50), "short");
    }

    /// The header-value rule that the wire test below exists to catch: `http`
    /// rejects anything but visible ASCII there, so a title with an em dash in it
    /// would have failed the whole request had it gone in the header unbaked.
    #[test]
    fn only_visible_ascii_survives_a_header_value() {
        assert_eq!(ascii_header("looprs-26r"), "looprs-26r");
        assert_eq!(
            ascii_header("the title: still fits"),
            "the title: still fits",
            "spaces are legal in a header value and stay spaces"
        );
        assert_eq!(ascii_header("an \u{2014} em dash"), "an ? em dash");
        assert_eq!(ascii_header("caf\u{e9}"), "caf?");
        assert_eq!(ascii_header("party \u{1f389} face"), "party ? face");
    }

    /// **The actual wire format, against a listener on loopback.**
    ///
    /// Ignored by default because it binds a port — the suite's "no network" rule
    /// is about the outside world, but a bound socket is still a shared resource,
    /// so run it by hand with `cargo test -- --ignored`. What it buys is the one
    /// thing a pure test cannot reach: that ntfy is being spoken to the way ntfy is
    /// actually spoken. Message in the **body**, chrome in `Title` / `Priority` /
    /// `Tags`, the topic in the path. A `[json]` request body — the obvious way to
    /// write this, and the way an earlier draft did — passes every pure test and
    /// shows up on the phone as literal JSON.
    #[tokio::test]
    #[ignore = "binds a loopback port; run with `cargo test -- --ignored`"]
    async fn the_post_puts_the_message_in_the_body_and_the_chrome_in_the_headers() {
        let (base, seen) = spawn_responder(200, "");
        let sink = Ntfy::spawn(&base, "looprs-wire-test").expect("the sink builds");

        sink.notify(BeadDone {
            id: "looprs-26r".into(),
            title: "a title with \u{2014} non-ascii in it".into(),
        });

        let req = {
            assert!(
                wait_until(|| !seen.lock().unwrap().is_empty()).await,
                "nothing ever reached the listener"
            );
            seen.lock().unwrap()[0].clone()
        };
        let (head, body) = req
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("not a request I recognise: {req:?}"));
        // Header names arrive lower-cased: `HeaderName` normalises them.
        let head = head.to_lowercase();

        assert!(
            head.starts_with("post /looprs-wire-test "),
            "the topic goes in the path: {head}"
        );
        assert!(head.contains("title: looprs: looprs-26r"), "{head}");
        assert!(head.contains("priority: 3"), "{head}");
        assert!(head.contains("tags: white_check_mark"), "{head}");
        assert_eq!(
            body, "looprs-26r \u{2014} a title with \u{2014} non-ascii in it",
            "the body is the message, in full UTF-8, not JSON"
        );
    }

    /// The failure ladder, on a real socket: a refused post is tried
    /// [`POST_ATTEMPTS`] times and no more, and the giving-up is the sink's problem
    /// rather than the caller's.
    ///
    /// Ignored for the same reason as the format test, plus it sleeps once between
    /// attempts, so it costs [`RETRY_AFTER`] of wall clock.
    #[tokio::test]
    #[ignore = "binds a loopback port and sleeps between retries"]
    async fn a_refused_post_is_tried_a_bounded_number_of_times_and_then_stops() {
        let (base, seen) = spawn_responder(500, "topic does not exist");
        let client = reqwest::Client::new();
        let done = BeadDone {
            id: "looprs-8".into(),
            title: "unreachable".into(),
        };

        // `post` returns `()` even after every attempt failed: the ladder's whole
        // point is that failure ends here, in a `tracing::error!`, and not as an
        // error the beads loop has to do something about. And because it is
        // awaited, the connection count is final by the time it returns — no poll.
        post(&client, &ntfy_url(&base, "looprs-wire-test"), &done).await;

        // `>=` first, so the responder thread's own bookkeeping is not raced; then
        // the exact count, which is final because `post` has already returned.
        assert!(
            wait_until(|| seen.lock().unwrap().len() >= POST_ATTEMPTS as usize).await,
            "only {} of {POST_ATTEMPTS} attempts ever connected",
            seen.lock().unwrap().len()
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            POST_ATTEMPTS as usize,
            "exactly {POST_ATTEMPTS} connections: retried once, then stopped"
        );
    }

    /// **`main`'s own call, exercised.** The last link between an env var and a
    /// notification, and the one nothing else covers: `notifier_from_env` really
    /// hands back a live `Ntfy` when both settings are present, and really falls
    /// back to `Noop` when they are not.
    ///
    /// Ignored because it mutates the process environment, which every test in this
    /// binary shares — and because setting it is `unsafe` in edition 2024 for
    /// precisely that reason. The unsafe is contained by the ignore: nothing else
    /// runs concurrently with it under `cargo test -- --ignored`, and the two wire
    /// tests take their endpoint as an argument rather than from the environment.
    #[tokio::test]
    #[ignore = "mutates the process environment"]
    async fn the_environment_switches_the_real_sink_on_and_off() {
        // SAFETY: this test is `#[ignore]`d, so it runs only when the ignored set
        // is asked for, and nothing in that set reads these two variables.
        unsafe {
            std::env::set_var("LOOPRS_NTFY_URL", "http://127.0.0.1:1");
            std::env::set_var("LOOPRS_NTFY_TOPIC", "looprs-smoke");
        }
        let on = format!("{:?}", notifier_from_env());
        assert!(
            on.contains("Ntfy"),
            "a configured environment must produce the real sink, got {on}"
        );

        // SAFETY: as above; and the variables are removed rather than blanked so
        // the "off" reading is the unset case, not the blank one.
        unsafe {
            std::env::remove_var("LOOPRS_NTFY_URL");
            std::env::remove_var("LOOPRS_NTFY_TOPIC");
        }
        let off = format!("{:?}", notifier_from_env());
        assert!(
            off.contains("Noop"),
            "an unconfigured environment must produce the silent sink, got {off}"
        );
    }

    // ------------------- the loopback responder both wire tests use -------------------

    /// Answer every request with `status` + `body`, recording each request line.
    ///
    /// `std::net`, no test-only dependency, and a plain OS thread: the responder is
    /// the least interesting thing in the room and must not be able to shape what it
    /// is observing.
    fn spawn_responder(status: u16, body: &str) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mine = seen.clone();
        let reply = format!(
            "HTTP/1.1 {status} whatever\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let mut req = String::new();
                // Read to the end of the headers; the body is one line, and one
                // read has it. Bounded, so a client that opens and says nothing
                // cannot wedge the responder for the tests behind this one.
                for _ in 0..8 {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            req.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if req.contains("\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                // Recorded before the reply goes out, so a test that awaited the
                // response cannot observe the reply without the record being there.
                mine.lock().unwrap().push(req);
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });

        (format!("http://127.0.0.1:{port}"), seen)
    }

    /// Poll a condition rather than sleep for a guessed amount of time: a working
    /// notifier returns in milliseconds, a broken one fails in five seconds rather
    /// than never.
    async fn wait_until(ready: impl Fn() -> bool) -> bool {
        for _ in 0..250 {
            if ready() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }
}
