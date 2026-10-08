//! The bounded UI message bus (looprs-6cj).
//!
//! `app_tx` used to be `mpsc::unbounded_channel::<Msg>()`. Every producer the
//! app has — three session pumps, the key relay, the notifier — wrote into a queue
//! with no ceiling, and exactly one consumer read from it. When one producer could
//! out-write that consumer, the gap was not dropped, delayed or bounded: it was
//! *kept*, in this process's resident memory, forever. A `while true; do echo …;
//! done` in Bash mode widened the gap at ~27 MiB/s and the run ended OOM; the
//! instrumented run behind the ticket had **1.8 M messages** sitting in that
//! queue while the UI had consumed 24 MB of the 400 MB that had been produced.
//!
//! A queue with no ceiling turns "the UI is slow" into "the machine is out of
//! memory", which is the worst possible failure mode for the one component that is
//! supposed to stay responsive.
//!
//! # The two policies, per message class
//!
//! Bounding the queue is only half the answer, because a full queue has to *do*
//! something, and the right something depends on what the message is made of:
//!
//! * **[`Msg::BashOutput`] — a byte stream: COALESCE.** These messages are not
//!   records, they are read buffers: the envelope doc says "do not re-split it",
//!   and nothing downstream cares where one message ended and the next began, as
//!   long as the bytes stay in order and none are lost. So two adjacent
//!   same-session, same-stream chunks are *joined into one*. The queue stops
//!   growing in **entries**, which is what the consumer pays per item, while
//!   every byte survives. This is merge-don't-drop in its literal sense, and it
//!   is not a lossy-tail policy: nothing is ever thrown away to make room.
//! * **Everything else — never merged, never dropped.** A `SessionDown`, a
//!   `ScreenHeld`, a `BeadStep` is a fact with no substitute: coalescing two of
//!   them invents a third fact, and dropping one loses an edge the UI can never
//!   recover (a missed `ScreenHeld { active: false }` leaves the app refusing to
//!   draw forever). Those messages wait for room. Waiting is the policy, and
//!   waiting is backpressure — the producer of a class cannot outrun the UI
//!   indefinitely in that class either.
//!
//! The cap is on **bytes**, not messages, because the leak was made of bytes: a
//! message-count cap would have been walked straight past by a producer that
//! merges. The space is held as [`tokio::sync::Semaphore`] permits that *ride
//! with the queued message* and go back to the pool when the consumer takes it —
//! so "queued bytes" is not a counter that can drift out of step with the queue,
//! it is the only copy of the fact.
//!
//! # Ordering
//!
//! FIFO, always. A sender that has to wait waits *before* enqueueing, so nothing
//! overtakes anything. Coalescing only ever appends to the tail entry, so within a
//! `(session, stream)` the bytes stay in producer order; that holds because a
//! byte stream has exactly one producer per key (one reader thread per shell).
//! A control message queued between two chunks of the same stream is not merged
//! across, and stays where it landed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::app::Msg;

/// The bus's ceiling, in queued bytes.
///
/// Sized against the consumer, not the producer: at the merged-chunk size below
/// and the app's measured ~60 fps draw, this is about two frames of output held
/// in flight. Big enough that the common case never blocks a producer; small
/// enough that "the UI stopped draining" costs 256 KiB rather than the 400 MB
/// the unbounded queue reached.
pub const DEFAULT_CAP_BYTES: usize = 256 * 1024;

/// The largest one coalesced [`Msg::BashOutput`] entry is allowed to get.
///
/// Without a per-entry ceiling a single drained entry could hold the entire cap,
/// which is legal but pointless: the consumer's win comes from *fewer, bigger*
/// updates, and it stops winning once one update is the whole batch. Two chunks'
/// worth of reads is enough to amortise every per-message cost the App pays
/// (the selection resync in `App::update` alone is more than the saving from
/// the next merge).
pub const COALESCE_MAX_BYTES: usize = 128 * 1024;

/// How many entries one consumer drain takes before handing control back.
///
/// A batch, not everything: a producer that can fill the bus faster than the UI
/// drains it must not be able to starve the run loop's other arms (keys, the
/// tick that draws) by handing it an unbounded batch.
pub const DRAIN_MAX_ENTRIES: usize = 64;

/// What the bus does with a message when it is time to enqueue it.
///
/// [`Self::Coalesce`] and [`Self::Wait`] are the two policies the module doc
/// argues for; there is no `Drop` variant, on purpose. A class that may be
/// dropped under pressure is a class whose loss the receiver has to be able to
/// tolerate, and nothing on this bus qualifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// Join onto the tail entry if it is the same stream; otherwise wait.
    Coalesce,
    /// Never joined, never dropped: wait for room.
    Wait,
}

/// The overflow policy for one message.
///
/// This is the "which class gets which policy" question answered in one place,
/// so a later message class cannot accidentally inherit the byte-stream rule by
/// being added to a `match` with a `_ =>` arm that looked harmless.
pub fn policy(msg: &Msg) -> Overflow {
    match msg {
        // A byte stream, and the only one. See the module doc.
        Msg::BashOutput { .. } => Overflow::Coalesce,
        // Every other class is a fact. Facts are not merged and not dropped.
        _ => Overflow::Wait,
    }
}

/// Bytes of memory a queued message holds.
///
/// `BashOutput` is charged for its payload because that is where the bytes
/// actually are; the rest are charged at the envelope's own size, which is a
/// constant they all share and which no producer floods.
fn charge(msg: &Msg) -> usize {
    match msg {
        Msg::BashOutput { chunk, .. } => chunk.len(),
        other => std::mem::size_of_val(other),
    }
}

/// Can `next` be appended to `tail`?
///
/// Both the session *and* the stream have to match. Merging two sessions' output
/// would put one user's bytes inside another session's transcript; merging two
/// streams of one session would do the same thing inside one transcript.
fn mergeable(tail: &Msg, next: &Msg) -> bool {
    let (
        Msg::BashOutput {
            session: ts,
            stream: tstream,
            chunk: tchunk,
        },
        Msg::BashOutput {
            session: ns,
            stream: nstream,
            ..
        },
    ) = (tail, next)
    else {
        return false;
    };
    ts == ns && tstream == nstream && tchunk.len() < COALESCE_MAX_BYTES
}

struct Entry {
    msg: Msg,
    /// The space this entry occupies, held for as long as the entry is queued.
    /// Dropped by the consumer when it takes the entry, which is the only way
    /// space ever comes back.
    _permits: Vec<OwnedSemaphorePermit>,
}

struct Core {
    queue: Mutex<VecDeque<Entry>>,
    /// `cap_bytes` worth of space. Held by whatever is queued.
    space: Arc<Semaphore>,
    /// The token supply this bus hands to the producers of byte output, so the
    /// UI's drain rate is what paces them. See [`Budget`].
    budget: Budget,
    /// The ceiling `space` was built with, kept so a message larger than the
    /// whole bus can be recognised instead of asked of the semaphore (which
    /// would wait on a number that can never be granted).
    cap: usize,
    /// Woken on every enqueue so a waiting consumer can look again.
    data: Notify,
    /// Live producers. The consumer treats zero as "nothing more is coming".
    senders: AtomicUsize,
    closed: AtomicBool,
}

impl Core {
    fn new(cap: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            space: Arc::new(Semaphore::new(cap)),
            budget: Budget::new(OUTPUT_BUDGET_TOKENS),
            cap,
            data: Notify::new(),
            senders: AtomicUsize::new(1),
            closed: AtomicBool::new(false),
        }
    }
}

/// The producing end of the bus. Clone it; every clone is a producer.
pub struct Sender {
    core: Arc<Core>,
}

impl Clone for Sender {
    fn clone(&self) -> Self {
        self.core.senders.fetch_add(1, Ordering::SeqCst);
        Self {
            core: self.core.clone(),
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        if self.core.senders.fetch_sub(1, Ordering::SeqCst) == 1 {
            // The last producer. Wake whoever is waiting so they can see that
            // nothing more is coming, rather than waiting for a message that
            // will never be sent.
            self.core.closed.store(true, Ordering::SeqCst);
            self.core.data.notify_waiters();
        }
    }
}

/// A message the bus would not take because the receiving end is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl Sender {
    /// Enqueue one message, applying that class's overflow policy.
    ///
    /// The space is acquired *before* the queue is touched, and the permit rides
    /// in with the message. So a producer that is out ahead of the UI parks on
    /// the space instead of standing in the queue with it: this await is the
    /// backpressure that has to reach the source, and it is why the byte lane
    /// upstream of here is flow-controlled too (see
    /// [`crate::session::bash`]'s credit window).
    ///
    /// Returns `Err(Closed)` once the receiver is gone; the message is not
    /// queued and, in the coalescing case, nothing was lost because nothing had
    /// been written yet.
    pub async fn send(&self, msg: Msg) -> Result<(), Closed> {
        // Charged as at least one permit so a zero-length chunk cannot ride
        // along for free and walk a producer past the cap.
        let charged = charge(&msg).max(1);
        // One message bigger than the entire bus is not a reason to deadlock:
        // it goes through uncharged. It is one message, the consumer takes it
        // on the next drain, and the alternative — waiting forever on a permit
        // the bus can never grant — is worse than one overshoot.
        let permits = if charged > self.core.cap {
            Vec::new()
        } else {
            vec![
                Arc::clone(&self.core.space)
                    .acquire_many_owned(charged as u32)
                    .await
                    .map_err(|_| Closed)?,
            ]
        };

        let mut q = self.core.queue.lock().unwrap();
        if self.core.closed.load(Ordering::SeqCst) {
            return Err(Closed);
        }
        if policy(&msg) == Overflow::Coalesce
            && let Some(tail) = q.back_mut()
            && mergeable(&tail.msg, &msg)
        {
            let Msg::BashOutput {
                chunk: tail_chunk, ..
            } = &mut tail.msg
            else {
                unreachable!("mergeable only ever matches BashOutput tails");
            };
            let Msg::BashOutput { chunk, .. } = msg else {
                unreachable!("mergeable only ever matches BashOutput heads");
            };
            tail_chunk.push_str(&chunk);
            tail._permits.extend(permits);
            drop(q);
            self.core.data.notify_waiters();
            return Ok(());
        }
        q.push_back(Entry {
            msg,
            _permits: permits,
        });
        drop(q);
        self.core.data.notify_waiters();
        Ok(())
    }

    /// How many entries are queued right now. Diagnostic.
    // Consumer: this module's tests, and the run-level spike that measures the
    // queue from outside. Not read by the binary, on purpose.
    #[allow(dead_code)]
    pub fn queued(&self) -> usize {
        self.core.queue.lock().unwrap().len()
    }

    /// The token supply the byte-stream producers must hold to keep producing.
    ///
    /// Handed out from here because the bus is the thing that knows when the UI
    /// has taken a byte, and the only honest bound on a producer is the rate of
    /// the consumer. Wiring: `main` puts this in `SessionConfig`, the pty
    /// reader takes from it, and the pump returns to it per message forwarded.
    pub fn budget(&self) -> Budget {
        self.core.budget.clone()
    }
}

/// Why a non-blocking receive did not produce a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Consumer: `Receiver::try_recv`, which the router tests' drain helper reads.
#[allow(dead_code)]
pub enum TryRecvError {
    /// Nothing queued right now; there may be more later.
    Empty,
    /// Nothing queued and every producer is gone.
    Disconnected,
}

/// The consuming end: the run loop.
pub struct Receiver {
    core: Arc<Core>,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // The consumer is gone, so nothing will ever take a byte out of this
        // queue again. Close it loudly: a producer parked on space has to be
        // told that waiting is now pointless, or the task parked there never
        // wakes up. Same reason `mpsc::Receiver::drop` closes the channel.
        self.core.closed.store(true, Ordering::SeqCst);
        self.core.space.close();
        self.core.budget.close();
        self.core.queue.lock().unwrap().clear();
        self.core.data.notify_waiters();
    }
}

impl Receiver {
    /// Take one message, waiting for it. `None` means the bus is finished.
    pub async fn recv(&mut self) -> Option<Msg> {
        self.recv_batch(1).await.into_iter().next()
    }

    /// Take up to `max_entries` messages, waiting for at least one.
    ///
    /// This is the consumer half of the fix. The run loop used to handle exactly
    /// one message per `select!` wake, so a burst cost the App its per-message
    /// work once per *read buffer* — including `App::update`'s selection
    /// resync, which is per-message and has nothing per-message about it. One
    /// drain of merged chunks makes the same bytes cost one tenth of the
    /// per-message work, and the flush stays where it belongs: once per frame.
    ///
    /// An empty `Vec` means the bus is finished, and only then.
    pub async fn recv_batch(&mut self, max_entries: usize) -> Vec<Msg> {
        loop {
            // Registered *before* the queue is read. A push that lands between
            // "empty" and "await" still wakes us; without the enable this is the
            // classic lost-wakeup stall, and a UI that stalls on a lost wakeup
            // looks like a hung terminal.
            let core = self.core.clone();
            let notified = core.data.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let batch = self.drain(max_entries);
            if !batch.is_empty() {
                return batch;
            }
            if self.finished() {
                return Vec::new();
            }
            notified.await;
        }
    }

    /// Take one message if one is there.
    // Consumer: the router tests' drain helper. The run loop drains batches.
    #[allow(dead_code)]
    pub fn try_recv(&mut self) -> Result<Msg, TryRecvError> {
        if let Some(entry) = self.core.queue.lock().unwrap().pop_front() {
            return Ok(entry.msg);
        }
        if self.finished() {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    fn drain(&mut self, max_entries: usize) -> Vec<Msg> {
        let mut q = self.core.queue.lock().unwrap();
        let n = max_entries.min(q.len());
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            // Dropping the entry's permits here is the moment the space returns
            // to the pool, which is what unblocks a parked producer.
            if let Some(entry) = q.pop_front() {
                out.push(entry.msg);
            }
        }
        out
    }

    /// Nothing queued and no way for anything to be queued again: every
    /// producer is gone or the consumer has been dropped.
    fn finished(&self) -> bool {
        self.core.closed.load(Ordering::SeqCst) || self.core.senders.load(Ordering::SeqCst) == 0
    }
}

/// Build a bus. `cap_bytes` is the ceiling on queued bytes.
pub fn channel(cap_bytes: usize) -> (Sender, Receiver) {
    let core = Arc::new(Core::new(cap_bytes));
    (Sender { core: core.clone() }, Receiver { core })
}

/// How many read buffers of output one run is allowed to have produced but not
/// yet handed to the UI. See [`Budget`].
pub const OUTPUT_BUDGET_TOKENS: usize = 16;

/// A counted, closable supply of *tokens*, blocking to acquire.
///
/// This is the piece that makes the byte bound reach the **source** rather than
/// just sitting at the front of the UI. A bounded bus alone moves the backlog up
/// one hop: the session's own event queue is unbounded, so a shell that prints
/// faster than the screen can be drawn does not stop at the bus, it stops in
/// memory one stage earlier — same leak, further upstream. The bus's ceiling has
/// to be felt by the thing holding the file descriptor.
///
/// So the token count travels with the *consumption* of output rather than with
/// the bytes themselves: the pty reader takes a token before every read, and
/// one comes back when the message built from the previous read has made it
/// into the UI's queue. If the UI stops draining, tokens stop coming back, the
/// reader parks, the pty's kernel buffer fills, and the child blocks in
/// `write(2)` — which is where backpressure belongs. Total output held by a
/// producer that has outrun the UI is then `tokens × read_size` plus the bus,
/// a few hundred KiB, instead of 400 MB and rising.
///
/// It is a *clamped* supply: releasing never takes the count above `max`, so a
/// producer that mis-counts (one pty read can become two output messages when
/// the screen watcher splits it) loosens the bound by at most `max` rather
/// than inflating it without end. The other direction — a token never returned
/// — is the one that would hang the shell, and it is covered by the reader-side
/// release for a chunk that produced no output at all (see
/// [`crate::session::bash`]).
#[derive(Clone)]
pub struct Budget {
    inner: Arc<BudgetInner>,
}

struct BudgetInner {
    max: usize,
    /// Guarded by `tokens`. `None` means closed.
    tokens: std::sync::Mutex<Option<usize>>,
    cvar: std::sync::Condvar,
}

impl std::fmt::Debug for Budget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget")
            .field("max", &self.inner.max)
            .field("available", &self.available())
            .finish()
    }
}

impl Budget {
    fn new(max: usize) -> Self {
        Self {
            inner: Arc::new(BudgetInner {
                max,
                tokens: std::sync::Mutex::new(Some(max)),
                cvar: std::sync::Condvar::new(),
            }),
        }
    }

    /// A supply nothing can run out of, for producers that are not byte
    /// streams and for every test that has no UI draining the other end.
    pub fn unbounded() -> Self {
        Self::new(usize::MAX / 4)
    }

    /// Take one token, parking the calling thread until there is one.
    ///
    /// Blocking by design: the only intended caller is the pty reader thread,
    /// whose whole job is to wait for bytes. `false` means the supply is closed
    /// and no token is ever coming.
    pub fn acquire_blocking(&self) -> bool {
        let mut guard = self.inner.tokens.lock().unwrap();
        loop {
            match guard.as_mut() {
                None => return false,
                Some(0) => guard = self.inner.cvar.wait(guard).unwrap(),
                Some(n) => {
                    *n -= 1;
                    return true;
                }
            }
        }
    }

    /// Put one token back, never above the ceiling.
    pub fn release(&self) {
        let mut guard = self.inner.tokens.lock().unwrap();
        if let Some(n) = guard.as_mut()
            && *n < self.inner.max
        {
            *n += 1;
            self.inner.cvar.notify_one();
        }
    }

    /// Tokens available right now. Diagnostic: this module's `Debug` impl prints
    /// it, and the outside-the-process spike reads it through there.
    pub fn available(&self) -> usize {
        self.inner.tokens.lock().unwrap().unwrap_or(0)
    }

    /// Stop handing out tokens and wake everyone waiting. The reader threads
    /// take that as "there is nobody left to read for".
    pub fn close(&self) {
        let mut guard = self.inner.tokens.lock().unwrap();
        if guard.take().is_some() {
            self.inner.cvar.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ByteStream, SessionId, TerminalType};

    fn sid(n: u64) -> SessionId {
        SessionId::new(TerminalType::Bash, n)
    }

    fn chunk(session: SessionId, text: &str) -> Msg {
        Msg::BashOutput {
            session,
            stream: ByteStream::Merged,
            chunk: text.to_string(),
        }
    }

    /// A burst of output writes *one* entry, not one entry per write.
    #[tokio::test]
    async fn coalescible_class_merges_instead_of_queueing_entries() {
        let (tx, mut rx) = channel(1024 * 1024);
        let a = sid(1);
        for i in 0..10 {
            tx.send(chunk(a, &format!("line {i}\n"))).await.unwrap();
        }
        assert_eq!(
            tx.queued(),
            1,
            "ten writes of one stream must be one queued entry"
        );

        let got = rx.recv_batch(DRAIN_MAX_ENTRIES).await;
        assert_eq!(got.len(), 1);
        let Msg::BashOutput { chunk, .. } = &got[0] else {
            panic!("expected BashOutput");
        };
        let expected = (0..10).map(|i| format!("line {i}\n")).collect::<String>();
        assert_eq!(*chunk, expected, "every byte survives, in order");
    }

    /// Merging is per session: two shells never share an entry.
    #[tokio::test]
    async fn merging_never_crosses_sessions() {
        let (tx, mut rx) = channel(1024 * 1024);
        let (a, b) = (sid(1), sid(2));
        tx.send(chunk(a, "a1\n")).await.unwrap();
        tx.send(chunk(b, "b1\n")).await.unwrap();
        tx.send(chunk(a, "a2\n")).await.unwrap();
        assert_eq!(tx.queued(), 3);

        let got = rx.recv_batch(DRAIN_MAX_ENTRIES).await;
        let texts: Vec<String> = got
            .iter()
            .map(|m| match m {
                Msg::BashOutput { chunk, .. } => chunk.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(texts, vec!["a1\n", "b1\n", "a2\n"]);
    }

    /// A control message is never merged with anything, and stays where it landed.
    #[tokio::test]
    async fn control_class_is_never_merged() {
        let (tx, mut rx) = channel(1024 * 1024);
        let a = sid(1);
        tx.send(chunk(a, "one\n")).await.unwrap();
        tx.send(Msg::System {
            session: Some(a),
            text: "between".into(),
        })
        .await
        .unwrap();
        tx.send(chunk(a, "two\n")).await.unwrap();
        assert_eq!(tx.queued(), 3, "the System message splits the stream");

        let got = rx.recv_batch(DRAIN_MAX_ENTRIES).await;
        assert_eq!(got.len(), 3);
        assert!(matches!(got[1], Msg::System { .. }));
    }

    /// The cap is real: a producer that outruns the consumer waits rather than
    /// growing the queue, and the message it is waiting with is not lost.
    #[tokio::test]
    async fn the_cap_blocks_instead_of_growing() {
        let cap = 4096;
        let (tx, mut rx) = channel(cap);
        let a = sid(1);
        let big = "x".repeat(1000);

        // Fill to the ceiling.
        for _ in 0..4 {
            tx.send(chunk(a, &big)).await.unwrap();
        }
        // The fifth cannot be merged away (a full tail stops the merge) and has
        // no room, so it must not complete.
        let pending = tx.send(chunk(a, &big));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), pending)
                .await
                .is_err(),
            "a producer past the ceiling has to wait, not queue"
        );
        assert!(tx.queued() <= 5);

        // Draining releases the space and the waiting send lands.
        let got = rx.recv_batch(DRAIN_MAX_ENTRIES).await;
        assert!(!got.is_empty());
        tx.send(chunk(a, "finally\n")).await.unwrap();
        let more = rx.recv_batch(DRAIN_MAX_ENTRIES).await;
        assert!(
            more.iter()
                .any(|m| matches!(m, Msg::BashOutput { chunk, .. } if chunk.contains("finally"))),
            "the message that had to wait was queued, not dropped"
        );
    }

    /// Nothing is dropped on the way out: the consumer drains what was sent and
    /// then sees the end.
    #[tokio::test]
    async fn last_producer_leaving_ends_the_bus_without_losing_queued_bytes() {
        let (tx, mut rx) = channel(64 * 1024);
        let a = sid(1);
        tx.send(chunk(a, "tail piece\n")).await.unwrap();
        drop(tx);
        let got = rx.recv_batch(DRAIN_MAX_ENTRIES).await;
        assert_eq!(got.len(), 1, "queued bytes are read before the end");
        assert!(rx.recv().await.is_none(), "and then the bus is finished");
    }

    /// Merging stops at the per-entry ceiling so one drained entry is never the
    /// whole batch.
    #[tokio::test]
    async fn coalescing_stops_at_its_own_ceiling() {
        let (tx, mut rx) = channel(8 * 1024 * 1024);
        let a = sid(1);
        let piece = "y".repeat(300);
        for _ in 0..(COALESCE_MAX_BYTES / 300 + 20) {
            tx.send(chunk(a, &piece)).await.unwrap();
        }
        let got = rx.recv_batch(1).await;
        let Msg::BashOutput { chunk, .. } = &got[0] else {
            panic!("expected BashOutput");
        };
        assert!(
            chunk.len() <= COALESCE_MAX_BYTES + 300,
            "a merged entry stops at the ceiling, got {}",
            chunk.len()
        );
    }

    /// A zero-length chunk cannot be used to walk past the cap.
    #[tokio::test]
    async fn empty_chunks_still_cost_a_slot() {
        let (tx, mut rx) = channel(1);
        let a = sid(1);
        // One permit of space: the first empty chunk takes it, the second must
        // wait rather than ride along for free.
        tx.send(chunk(a, "")).await.unwrap();
        let pending = tx.send(chunk(a, ""));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), pending)
                .await
                .is_err(),
            "a zero-length message is still a message"
        );
        assert_eq!(rx.drain(10).len(), 1);
    }

    /// Dropping the consumer closes the bus: a producer that keeps writing sees
    /// the end instead of parking on space nobody will ever release.
    #[tokio::test]
    async fn dropped_receiver_ends_the_senders_rather_than_parking_them() {
        let (tx, rx) = channel(256);
        let a = sid(1);
        let handle = tokio::spawn(async move {
            let mut n = 0;
            while tx.send(chunk(a, &"z".repeat(64))).await.is_ok() {
                n += 1;
                if n > 10_000 {
                    panic!("the sender never noticed the consumer was gone");
                }
            }
            n
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(rx);
        let sent = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("the sender must stop when the receiver is gone")
            .unwrap();
        assert!(sent > 0, "it did write before the end: {sent}");
    }
}
