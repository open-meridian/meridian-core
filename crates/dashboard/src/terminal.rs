//! Sessions for a terminal: W6.13 and W6.14.
//!
//! `meridian connect` listens on a loopback port and opens the dashboard's
//! terminal sign-in with a PKCE challenge (RFC 7636) and a state. The person
//! signs in afresh -- never on a browser session they already hold -- and
//! confirms; the browser is sent back to the loopback address with a one-time
//! code; the CLI exchanges the code, with its verifier, for a session of its
//! own (RFC 8252). This module is that exchange's memory, and nothing about
//! HTTP.
//!
//! Three things are held, each bounded so that nobody can fill this process
//! by asking:
//!
//! - **Requests** a terminal opened and nobody has finished: 10 minutes, and
//!   at most [`MAX_REQUESTS`], the oldest dropped first. Opening one needs no
//!   sign-in, which is why the cap exists.
//! - **Codes**: 60 seconds, spent by their first use. A code presented twice
//!   ends the session its first use made, because the only way to present it
//!   twice is for somebody else to have it too.
//! - **Sessions**: decisions/015's bounds, counted from the sign-in. Held by
//!   the SHA-256 of the token and never the token, so a dump of this process
//!   signs nobody in. What ended a session is remembered until it would have
//!   lapsed anyway, so the CLI can say which.
//!
//! A session names a person and nothing they may do. Access is evaluated per
//! request from the records, exactly as a browser's is.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::clock::{MINUTE_NS, SECOND_NS};
use crate::session::{token, ABSOLUTE_NS, IDLE_NS};

/// How long a terminal's request waits for somebody to sign in and confirm.
pub const REQUEST_NS: i64 = 10 * MINUTE_NS;
/// How long a code waits to be exchanged. The CLI is listening when it
/// arrives, so this only has to cover a slow machine.
pub const CODE_NS: i64 = 60 * SECOND_NS;
/// Requests held at once. Far above any firm's staff connecting at the same
/// moment, and far below what would matter to this process's memory.
pub const MAX_REQUESTS: usize = 1_000;

/// What a terminal asked for, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub redirect_uri: String,
    pub challenge: String,
    pub state: String,
}

/// Who signed in to a terminal's request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub subject: String,
    pub display_name: String,
    pub directory_groups: Vec<String>,
    pub signed_in_at_ns: i64,
}

/// What an exchange hands the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issued {
    /// The only copy there will be.
    pub session: String,
    pub subject: String,
    pub expires_at_ns: i64,
}

/// Why a session was not honoured. Said to the CLI, which says it to the
/// person: a lapsed session and an ended one ask different things of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Past one of its bounds.
    Lapsed,
    /// Signed out, or ended by a deployment admin.
    Ended,
    /// Never issued here -- or issued before this dashboard restarted.
    Unknown,
}

impl Refusal {
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::Lapsed => "lapsed",
            Refusal::Ended => "ended",
            Refusal::Unknown => "unknown",
        }
    }
}

/// Check what a terminal asked for, before anybody is asked to sign in.
///
/// The redirect is to the loopback interface and nowhere else: `http`, the
/// literal `127.0.0.1` or `[::1]`, any port, and the path `/callback`. Not
/// `localhost`, which a hosts file can send elsewhere, and never a host a
/// link could name. The state comes back in a URL unescaped, so it is held to
/// the characters that need no escaping.
pub fn check(
    redirect_uri: &str,
    challenge: &str,
    method: &str,
    state: &str,
) -> Result<Request, String> {
    if !loopback(redirect_uri) {
        return Err(
            "the terminal's address must be http://127.0.0.1:<port>/callback or \
             http://[::1]:<port>/callback"
                .into(),
        );
    }
    if method != "S256" {
        return Err("the code challenge method must be S256".into());
    }
    // A SHA-256 in unpadded base64url is 43 characters, always.
    if challenge.len() != 43 || !challenge.bytes().all(url_safe) {
        return Err("the code challenge is not a SHA-256 in base64url".into());
    }
    if state.is_empty() || state.len() > 256 || !state.bytes().all(unreserved) {
        return Err("the state must be 1 to 256 unreserved characters".into());
    }
    Ok(Request {
        redirect_uri: redirect_uri.to_string(),
        challenge: challenge.to_string(),
        state: state.to_string(),
    })
}

fn loopback(uri: &str) -> bool {
    let Some(rest) = uri.strip_prefix("http://") else {
        return false;
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return false;
    };
    if path != "callback" {
        return false;
    }
    let port = if let Some(port) = authority.strip_prefix("127.0.0.1:") {
        port
    } else if let Some(port) = authority.strip_prefix("[::1]:") {
        port
    } else {
        return false;
    };
    matches!(port.parse::<u16>(), Ok(port) if port > 0) && !port.starts_with('0')
}

fn url_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

fn unreserved(byte: u8) -> bool {
    url_safe(byte) || byte == b'.' || byte == b'~'
}

/// RFC 7636's S256: the challenge is the verifier's SHA-256, base64url.
fn verifies(verifier: &str, challenge: &str) -> bool {
    if !(43..=128).contains(&verifier.len()) || !verifier.bytes().all(unreserved) {
        return false;
    }
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    same(computed.as_bytes(), challenge.as_bytes())
}

/// Equal, in a time that does not depend on where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

fn hashed(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

struct Waiting {
    request: Request,
    opened_at_ns: i64,
    /// Set once somebody has signed in to it, with the token their
    /// confirmation form carries.
    signed_in: Option<(Person, String)>,
}

struct Code {
    request: Request,
    person: Person,
    issued_at_ns: i64,
    /// The session its first use made, by hash.
    redeemed: Option<String>,
}

struct Held {
    person: Person,
    last_seen_at_ns: i64,
}

#[derive(Default)]
struct Inner {
    waiting: HashMap<String, Waiting>,
    /// A provider's sign-in state, to the request it is signing somebody in
    /// to. A provider sends the browser back to one address for every
    /// sign-in, and this is how that address tells a terminal's from a
    /// browser's.
    by_provider_state: HashMap<String, String>,
    codes: HashMap<String, Code>,
    sessions: HashMap<String, Held>,
    /// Why a session ended, until it would have lapsed anyway.
    gone: HashMap<String, (Refusal, i64)>,
}

#[derive(Default)]
pub struct Terminals {
    inner: Mutex<Inner>,
}

impl Terminals {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("terminal lock poisoned")
    }

    /// Hold a checked request, and return the id the sign-in carries.
    pub fn open(&self, request: Request, now_ns: i64) -> String {
        let mut inner = self.lock();
        inner
            .waiting
            .retain(|_, w| now_ns - w.opened_at_ns <= REQUEST_NS);
        while inner.waiting.len() >= MAX_REQUESTS {
            let oldest = inner
                .waiting
                .iter()
                .min_by_key(|(_, w)| w.opened_at_ns)
                .map(|(id, _)| id.clone())
                .expect("a full map has an oldest");
            inner.waiting.remove(&oldest);
        }
        let id = token();
        inner.waiting.insert(
            id.clone(),
            Waiting {
                request,
                opened_at_ns: now_ns,
                signed_in: None,
            },
        );
        id
    }

    /// Note that a provider sign-in with this state is for this request.
    pub fn through_provider(&self, provider_state: &str, id: &str) {
        self.lock()
            .by_provider_state
            .insert(provider_state.to_string(), id.to_string());
    }

    /// The request a provider sign-in was for, if it was for one. Taken, so
    /// a state is matched once.
    pub fn for_provider_state(&self, provider_state: &str) -> Option<String> {
        self.lock().by_provider_state.remove(provider_state)
    }

    /// Somebody signed in to this request. Returns the token their
    /// confirmation must carry, or nothing when the request has gone -- too
    /// old, or already signed in to.
    pub fn signed_in(&self, id: &str, person: Person, now_ns: i64) -> Option<String> {
        let mut inner = self.lock();
        let waiting = inner.waiting.get_mut(id)?;
        if now_ns - waiting.opened_at_ns > REQUEST_NS || waiting.signed_in.is_some() {
            return None;
        }
        let confirm = token();
        waiting.signed_in = Some((person, confirm.clone()));
        Some(confirm)
    }

    /// The person confirmed: a code, and where to send it. Or declined:
    /// where to say so. Either way the request is spent.
    pub fn decide(
        &self,
        id: &str,
        confirm: &str,
        allow: bool,
        now_ns: i64,
    ) -> Result<(Request, Option<String>), &'static str> {
        let mut inner = self.lock();
        let Some(waiting) = inner.waiting.get(id) else {
            return Err("this terminal sign-in has expired or was already used");
        };
        let Some((_, expected)) = &waiting.signed_in else {
            return Err("nobody has signed in to this terminal sign-in");
        };
        if !same(expected.as_bytes(), confirm.as_bytes()) {
            return Err("this confirmation did not come from this sign-in");
        }
        let waiting = inner.waiting.remove(id).expect("found above");
        if now_ns - waiting.opened_at_ns > REQUEST_NS {
            return Err("this terminal sign-in has expired or was already used");
        }
        if !allow {
            return Ok((waiting.request, None));
        }
        let (person, _) = waiting.signed_in.expect("checked above");
        let code = token();
        inner.codes.insert(
            code.clone(),
            Code {
                request: waiting.request.clone(),
                person,
                issued_at_ns: now_ns,
                redeemed: None,
            },
        );
        Ok((waiting.request, Some(code)))
    }

    /// Trade a code for a session. Every refusal is the same refusal, as
    /// RFC 6749 has it; which check failed is for the log.
    pub fn exchange(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        now_ns: i64,
    ) -> Result<Issued, &'static str> {
        let mut inner = self.lock();
        let Some(held) = inner.codes.get(code) else {
            return Err("unknown code");
        };
        if let Some(first) = held.redeemed.clone() {
            // Presented twice. Somebody else has it, so what it bought is
            // theirs too, and ends.
            let signed_in_at_ns = held.person.signed_in_at_ns;
            inner.sessions.remove(&first);
            inner
                .gone
                .insert(first, (Refusal::Ended, signed_in_at_ns + ABSOLUTE_NS));
            return Err("code used twice; the session it made is ended");
        }
        let held = inner.codes.remove(code).expect("found above");
        if now_ns - held.issued_at_ns > CODE_NS {
            return Err("code expired");
        }
        if held.request.redirect_uri != redirect_uri {
            return Err("redirect_uri differs from the one the code was issued to");
        }
        if !verifies(verifier, &held.request.challenge) {
            return Err("verifier does not match the challenge");
        }
        let session = token();
        let key = hashed(&session);
        let expires_at_ns = held.person.signed_in_at_ns + ABSOLUTE_NS;
        let subject = held.person.subject.clone();
        inner.codes.insert(
            code.to_string(),
            Code {
                redeemed: Some(key.clone()),
                ..held
            },
        );
        let person = inner.codes[code].person.clone();
        inner.sessions.insert(
            key,
            Held {
                person,
                last_seen_at_ns: now_ns,
            },
        );
        Ok(Issued {
            session,
            subject,
            expires_at_ns,
        })
    }

    /// The person behind a session, touched; or why not.
    pub fn find(&self, session: &str, now_ns: i64) -> Result<Person, Refusal> {
        let key = hashed(session);
        let mut inner = self.lock();
        if let Some(held) = inner.sessions.get_mut(&key) {
            let idle = now_ns - held.last_seen_at_ns > IDLE_NS;
            let old = now_ns - held.person.signed_in_at_ns > ABSOLUTE_NS;
            if !(idle || old) {
                held.last_seen_at_ns = now_ns;
                return Ok(held.person.clone());
            }
            let until = held.person.signed_in_at_ns + ABSOLUTE_NS;
            inner.sessions.remove(&key);
            inner.gone.insert(key, (Refusal::Lapsed, until));
            return Err(Refusal::Lapsed);
        }
        Err(inner
            .gone
            .get(&key)
            .map(|(why, _)| *why)
            .unwrap_or(Refusal::Unknown))
    }

    /// `meridian sign-out`. Ending a session that has already gone is not
    /// an error: the CLI forgets it either way.
    pub fn end(&self, session: &str) {
        let key = hashed(session);
        let mut inner = self.lock();
        if let Some(held) = inner.sessions.remove(&key) {
            let until = held.person.signed_in_at_ns + ABSOLUTE_NS;
            inner.gone.insert(key, (Refusal::Ended, until));
        }
    }

    /// End every terminal session a person holds, and say how many.
    pub fn end_person(&self, subject: &str) -> usize {
        let mut inner = self.lock();
        let theirs: Vec<String> = inner
            .sessions
            .iter()
            .filter(|(_, held)| held.person.subject == subject)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &theirs {
            let held = inner.sessions.remove(key).expect("listed above");
            let until = held.person.signed_in_at_ns + ABSOLUTE_NS;
            inner.gone.insert(key.clone(), (Refusal::Ended, until));
        }
        theirs.len()
    }

    /// Who holds a live terminal session, how many, by name -- for the admin
    /// page. Nothing about the sessions themselves.
    pub fn holders(&self, now_ns: i64) -> Vec<(String, String, usize)> {
        let inner = self.lock();
        let mut counted: HashMap<&str, (&str, usize)> = HashMap::new();
        for held in inner.sessions.values() {
            let live = now_ns - held.last_seen_at_ns <= IDLE_NS
                && now_ns - held.person.signed_in_at_ns <= ABSOLUTE_NS;
            if live {
                counted
                    .entry(held.person.subject.as_str())
                    .or_insert((held.person.display_name.as_str(), 0))
                    .1 += 1;
            }
        }
        let mut holders: Vec<_> = counted
            .into_iter()
            .map(|(subject, (name, n))| (subject.to_string(), name.to_string(), n))
            .collect();
        holders.sort();
        holders
    }

    /// Forget everything past its bound.
    pub fn sweep(&self, now_ns: i64) {
        let mut inner = self.lock();
        inner
            .waiting
            .retain(|_, w| now_ns - w.opened_at_ns <= REQUEST_NS);
        let waiting: std::collections::HashSet<String> = inner.waiting.keys().cloned().collect();
        inner.by_provider_state.retain(|_, id| waiting.contains(id));
        inner
            .codes
            .retain(|_, c| now_ns - c.issued_at_ns <= CODE_NS);
        let mut lapsed = Vec::new();
        inner.sessions.retain(|key, held| {
            let live = now_ns - held.last_seen_at_ns <= IDLE_NS
                && now_ns - held.person.signed_in_at_ns <= ABSOLUTE_NS;
            if !live {
                lapsed.push((key.clone(), held.person.signed_in_at_ns + ABSOLUTE_NS));
            }
            live
        });
        for (key, until) in lapsed {
            inner.gone.insert(key, (Refusal::Lapsed, until));
        }
        inner.gone.retain(|_, (_, until)| now_ns <= *until);
    }
}

/// A moment as RFC 3339 in UTC, to the second. Here rather than a date
/// crate, because this is the one date the dashboard writes.
pub fn rfc3339(at_ns: i64) -> String {
    let seconds = at_ns.div_euclid(SECOND_NS);
    let (days, of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Howard Hinnant's days-from-civil, inverted.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3_600,
        of_day % 3_600 / 60,
        of_day % 60
    )
}

#[cfg(test)]
mod tests;
