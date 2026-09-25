// SPDX-License-Identifier: Apache-2.0
//! The egress resolver: a DNS server of Zygo's own, inside the sandbox.
//!
//! An allowlist by *name* cannot be enforced by a packet filter, which only
//! ever sees addresses — and the addresses behind `api.example.com` are the
//! service's to change, not ours to know in advance. The design's answer is to
//! make the sandbox's resolver the thing that decides: every query passes
//! through here, a name the allowlist does not cover does not resolve, and a
//! name it does cover is resolved on the host and its addresses are added to
//! the nftables set *before* the answer goes back. By the time the sandbox can
//! act on an address, the filter already permits it.
//!
//! That is also what makes `*.example.com` work: the wildcard is matched
//! against the name asked for, not turned into addresses ahead of time.
//!
//! The server is deliberately small. UDP only; `A` and `AAAA` answered; every
//! other type gets an empty `NOERROR` so a client falls back to those; no
//! compression parsed in questions (no resolver sends it), one pointer used in
//! answers. It binds **inside the sandbox's network namespace**, on the
//! loopback address the sandbox's `resolv.conf` names, because `resolv.conf`
//! cannot name a port and so the resolver has to be on port 53 of an address
//! the sandbox can reach — which no host-side socket is.
//!
//! What it is not: a cache, a recursive resolver, or a DNS-over-anything
//! endpoint. Resolution itself is the host's `getaddrinfo`, and that is the
//! point — the host's resolver configuration stays on the host.

use std::net::{IpAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::spec::AllowRule;

pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

/// Time to live on every answer. Short, so a client comes back here rather
/// than caching an address past the filter's own timeout on it.
pub const ANSWER_TTL: u32 = 30;

/// Largest packet accepted or produced. Every answer here fits with room: the
/// question plus a handful of addresses.
const MAX_PACKET: usize = 512;

/// One question, as much of it as the resolver needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub id: u16,
    /// Lower-case, no trailing dot.
    pub name: String,
    pub qtype: u16,
    /// The question section, byte for byte, to echo in the answer.
    question: Vec<u8>,
    /// Recursion desired, echoed back.
    rd: bool,
}

/// Parse a query. `None` for anything that is not one well-formed question,
/// which the server ignores rather than answering — there is no id to answer
/// under if the header itself is broken.
pub fn parse_query(packet: &[u8]) -> Option<Query> {
    if packet.len() < 12 {
        return None;
    }
    let id = u16::from_be_bytes([packet[0], packet[1]]);
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if flags & 0x8000 != 0 {
        return None; // a response, not a query
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]);
    if qdcount != 1 {
        return None;
    }

    let mut labels = Vec::new();
    let mut at = 12;
    loop {
        let len = *packet.get(at)? as usize;
        at += 1;
        if len == 0 {
            break;
        }
        if len & 0xC0 != 0 {
            return None; // compression in a question: nothing sends this
        }
        let label = packet.get(at..at + len)?;
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        at += len;
        if labels.len() > 127 {
            return None;
        }
    }
    let qtype = u16::from_be_bytes([*packet.get(at)?, *packet.get(at + 1)?]);
    let qclass = u16::from_be_bytes([*packet.get(at + 2)?, *packet.get(at + 3)?]);
    if qclass != CLASS_IN {
        return None;
    }
    let question = packet[12..at + 4].to_vec();

    Some(Query {
        id,
        name: labels.join("."),
        qtype,
        question,
        rd: flags & 0x0100 != 0,
    })
}

/// Response codes used here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rcode {
    NoError = 0,
    /// The name is not on the allowlist. `NXDOMAIN` rather than `REFUSED`,
    /// because a client treats `REFUSED` as "try the next server" and there
    /// is none; "does not exist" is final and is also true from in here.
    NxDomain = 3,
}

/// Build the answer to `query`: the question echoed, then one record per
/// address of the queried type.
pub fn response(query: &Query, rcode: Rcode, addrs: &[IpAddr]) -> Vec<u8> {
    let mut answers: Vec<&IpAddr> = addrs
        .iter()
        .filter(|a| match query.qtype {
            TYPE_A => a.is_ipv4(),
            TYPE_AAAA => a.is_ipv6(),
            _ => false,
        })
        .collect();

    // How many records actually fit, decided *before* the header is written.
    //
    // ANCOUNT used to be `answers.len()` while the loop below stopped at the
    // packet limit, so a name with enough addresses produced a header
    // promising records that were not there — a malformed answer that a
    // resolver reports as a failed lookup rather than a short one
    // (B-16, the code review).
    //
    // The truncation bit is deliberately *not* set. It tells a client to retry
    // over TCP, and this resolver listens on UDP only (see `serve`), so a
    // client that obeyed would get no answer at all instead of a usable
    // subset. A sandbox that needs every address of a thirty-address name is
    // not a case this resolver is for.
    let fits = usable_answers(query, &answers);
    answers.truncate(fits);

    let mut out = Vec::with_capacity(MAX_PACKET);
    out.extend_from_slice(&query.id.to_be_bytes());
    // QR, AA, RD as asked, RA: this is the sandbox's only resolver and it is
    // authoritative for what the sandbox may reach.
    let mut flags: u16 = 0x8000 | 0x0400 | 0x0080;
    if query.rd {
        flags |= 0x0100;
    }
    flags |= rcode as u16;
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out.extend_from_slice(&(answers.len() as u16).to_be_bytes()); // ANCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    out.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT
    out.extend_from_slice(&query.question);

    for addr in answers {
        // A pointer to the name in the question, at offset 12.
        out.extend_from_slice(&[0xC0, 0x0C]);
        out.extend_from_slice(&query.qtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&ANSWER_TTL.to_be_bytes());
        match addr {
            IpAddr::V4(v4) => {
                out.extend_from_slice(&4u16.to_be_bytes());
                out.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                out.extend_from_slice(&16u16.to_be_bytes());
                out.extend_from_slice(&v6.octets());
            }
        }
    }
    out
}

/// How many of `answers` fit in one packet, given this query's question.
///
/// A record is a two-byte name pointer, type, class, a four-byte TTL, a
/// two-byte length and the address itself: 16 bytes for A, 28 for AAAA. The
/// header is twelve and the question is echoed whole.
fn usable_answers(query: &Query, answers: &[&IpAddr]) -> usize {
    const HEADER: usize = 12;
    let fixed = HEADER + query.question.len();
    let each = match query.qtype {
        TYPE_AAAA => 28,
        _ => 16,
    };
    let room = MAX_PACKET.saturating_sub(fixed) / each;
    room.min(answers.len())
}

/// What the allowlist says about a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The ports the matching rules permit; `None` in the list means any port.
    /// Empty means no rule matched.
    pub ports: Vec<Option<u16>>,
}

impl Verdict {
    pub fn allowed(&self) -> bool {
        !self.ports.is_empty()
    }
}

/// Match a name against the allowlist. Every matching rule contributes its
/// port, because `api.example.com:443` and `*.example.com:8443` may both apply.
pub fn verdict(rules: &[AllowRule], name: &str) -> Verdict {
    let mut ports: Vec<Option<u16>> = rules
        .iter()
        .filter(|r| r.matches_host(name))
        .map(|r| r.port)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    Verdict { ports }
}

/// Where a resolved address goes before the answer is sent: the caller adds it
/// to the filter for these ports. Returning an error means the address must
/// not be handed out — an answer the filter does not yet permit would only
/// send the client into a connect timeout.
pub type Admit<'a> = dyn FnMut(IpAddr, &[Option<u16>]) -> std::io::Result<()> + Send + 'a;

/// Resolve a name on the host.
pub type Resolve<'a> = dyn Fn(&str) -> Vec<IpAddr> + Send + Sync + 'a;

/// Answer one query, deciding, resolving and admitting. Pure apart from what
/// the two callbacks do, so it is testable with fakes for both.
pub fn answer(
    query: &Query,
    rules: &[AllowRule],
    allow_private: bool,
    resolve: &Resolve<'_>,
    admit: &mut Admit<'_>,
) -> Vec<u8> {
    let verdict = verdict(rules, &query.name);
    if !verdict.allowed() {
        return response(query, Rcode::NxDomain, &[]);
    }
    if query.qtype != TYPE_A && query.qtype != TYPE_AAAA {
        return response(query, Rcode::NoError, &[]);
    }

    let mut addrs = resolve(&query.name);
    if !allow_private {
        // The filter rejects these ranges above every allow rule, so an answer
        // naming one would be a connect that fails. Leaving it out is the
        // same policy, one round trip earlier.
        addrs.retain(|a| !is_private(*a));
    }
    addrs.sort();
    addrs.dedup();

    let mut admitted = Vec::with_capacity(addrs.len());
    for addr in addrs {
        match admit(addr, &verdict.ports) {
            Ok(()) => admitted.push(addr),
            Err(e) => tracing::warn!(name = %query.name, %addr, "could not admit an address: {e}"),
        }
    }
    response(query, Rcode::NoError, &admitted)
}

/// The same ranges the ruleset rejects and the spec's validator refuses.
///
/// One definition, in `spec::types`, because three copies disagreed (B-18).
pub fn is_private(addr: IpAddr) -> bool {
    crate::spec::types::is_private_addr(addr)
}

/// Serve on `socket` until `stop` is set. Blocks; run it on its own thread.
///
/// The read timeout is what makes `stop` take effect: the loop wakes four
/// times a second to look at it, which is invisible to a client and costs
/// nothing when idle.
pub fn serve(
    socket: UdpSocket,
    rules: Vec<AllowRule>,
    allow_private: bool,
    stop: Arc<AtomicBool>,
    resolve: &Resolve<'_>,
    admit: &mut Admit<'_>,
) {
    if socket
        .set_read_timeout(Some(Duration::from_millis(250)))
        .is_err()
    {
        return;
    }
    let mut buf = [0u8; MAX_PACKET];
    while !stop.load(Ordering::Relaxed) {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => return,
        };
        let Some(query) = parse_query(&buf[..n]) else {
            continue;
        };
        let reply = answer(&query, &rules, allow_private, resolve, admit);
        let _ = socket.send_to(&reply, from);
    }
}

/// Resolve on the host through `getaddrinfo`, which is what the rest of the
/// host uses and which honours whatever it is configured with.
pub fn resolve_on_host(name: &str) -> Vec<IpAddr> {
    use std::net::ToSocketAddrs;
    (name, 0)
        .to_socket_addrs()
        .map(|it| it.map(|s| s.ip()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(list: &[&str]) -> Vec<AllowRule> {
        list.iter().map(|s| s.parse().expect("rule")).collect()
    }

    /// A query packet for `name`, as a stub resolver would send it.
    fn query_packet(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut p = vec![];
        p.extend_from_slice(&id.to_be_bytes());
        p.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
        p.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            p.push(label.len() as u8);
            p.extend_from_slice(label.as_bytes());
        }
        p.push(0);
        p.extend_from_slice(&qtype.to_be_bytes());
        p.extend_from_slice(&CLASS_IN.to_be_bytes());
        p
    }

    #[test]
    fn a_question_is_parsed_and_lower_cased() {
        let q = parse_query(&query_packet(0x1234, "API.Example.COM", TYPE_A)).expect("query");
        assert_eq!(q.id, 0x1234);
        assert_eq!(q.name, "api.example.com");
        assert_eq!(q.qtype, TYPE_A);
        assert!(q.rd);
    }

    #[test]
    fn garbage_and_responses_are_not_questions() {
        assert!(parse_query(b"").is_none());
        assert!(parse_query(&[0u8; 11]).is_none());
        let mut p = query_packet(1, "a.b", TYPE_A);
        p[2] |= 0x80; // QR: a response
        assert!(parse_query(&p).is_none());
        let mut two = query_packet(1, "a.b", TYPE_A);
        two[5] = 2; // QDCOUNT 2
        assert!(parse_query(&two).is_none());
        let mut truncated = query_packet(1, "a.b", TYPE_A);
        truncated.truncate(15);
        assert!(parse_query(&truncated).is_none());
    }

    #[test]
    fn the_answer_echoes_the_question_and_points_at_it() {
        let q = parse_query(&query_packet(7, "api.example.com", TYPE_A)).unwrap();
        let r = response(&q, Rcode::NoError, &["93.184.216.34".parse().unwrap()]);
        assert_eq!(&r[0..2], &7u16.to_be_bytes());
        assert_eq!(r[2] & 0x80, 0x80, "QR");
        assert_eq!(r[2] & 0x04, 0x04, "AA");
        assert_eq!(r[3] & 0x0F, 0, "rcode");
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1, "one answer");
        // The question, verbatim.
        assert_eq!(&r[12..12 + q.question.len()], &q.question[..]);
        let rr = &r[12 + q.question.len()..];
        assert_eq!(&rr[0..2], &[0xC0, 0x0C], "pointer to the name");
        assert_eq!(u16::from_be_bytes([rr[2], rr[3]]), TYPE_A);
        assert_eq!(u32::from_be_bytes([rr[6], rr[7], rr[8], rr[9]]), ANSWER_TTL);
        assert_eq!(u16::from_be_bytes([rr[10], rr[11]]), 4);
        assert_eq!(&rr[12..16], &[93, 184, 216, 34]);
    }

    #[test]
    fn an_aaaa_question_gets_only_v6_and_vice_versa() {
        let addrs: Vec<IpAddr> = vec![
            "93.184.216.34".parse().unwrap(),
            "2606:2800:220:1:248:1893:25c8:1946".parse().unwrap(),
        ];
        let a = parse_query(&query_packet(1, "x.y", TYPE_A)).unwrap();
        assert_eq!(
            u16::from_be_bytes([
                response(&a, Rcode::NoError, &addrs)[6],
                response(&a, Rcode::NoError, &addrs)[7]
            ]),
            1
        );
        let aaaa = parse_query(&query_packet(1, "x.y", TYPE_AAAA)).unwrap();
        let r = response(&aaaa, Rcode::NoError, &addrs);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1);
        assert_eq!(
            u16::from_be_bytes([r[r.len() - 18], r[r.len() - 17]]),
            16,
            "rdlength of an AAAA"
        );
    }

    #[test]
    fn a_name_off_the_list_does_not_exist() {
        let q = parse_query(&query_packet(1, "evil.example.net", TYPE_A)).unwrap();
        let resolve = |_: &str| vec!["1.2.3.4".parse().unwrap()];
        let mut admitted = Vec::new();
        let mut admit = |a: IpAddr, p: &[Option<u16>]| {
            admitted.push((a, p.to_vec()));
            Ok(())
        };
        let r = answer(
            &q,
            &rules(&["api.example.com:443"]),
            false,
            &resolve,
            &mut admit,
        );
        assert_eq!(r[3] & 0x0F, Rcode::NxDomain as u8);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 0, "no answers");
        assert!(admitted.is_empty(), "nothing was let through the filter");
    }

    #[test]
    fn an_allowed_name_is_admitted_before_it_is_answered() {
        // The order is the guarantee: by the time the client has an address,
        // the filter permits it.
        let q = parse_query(&query_packet(1, "api.example.com", TYPE_A)).unwrap();
        let resolve = |_: &str| vec!["93.184.216.34".parse().unwrap()];
        let mut log = Vec::new();
        let mut admit = |a: IpAddr, p: &[Option<u16>]| {
            log.push(format!("admit {a} {p:?}"));
            Ok(())
        };
        let r = answer(
            &q,
            &rules(&["api.example.com:443"]),
            false,
            &resolve,
            &mut admit,
        );
        assert_eq!(log, ["admit 93.184.216.34 [Some(443)]"]);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1);
    }

    #[test]
    fn a_wildcard_matches_subdomains_and_only_those() {
        let list = rules(&["*.example.com:443"]);
        assert!(verdict(&list, "api.example.com").allowed());
        assert!(verdict(&list, "deep.api.example.com").allowed());
        assert!(
            !verdict(&list, "example.com").allowed(),
            "the apex is not a subdomain"
        );
        assert!(!verdict(&list, "notexample.com").allowed());
        assert!(!verdict(&list, "example.com.evil.net").allowed());
    }

    #[test]
    fn every_matching_rule_contributes_its_port() {
        let list = rules(&[
            "api.example.com:443",
            "*.example.com:8443",
            "api.example.com",
        ]);
        let v = verdict(&list, "api.example.com");
        assert_eq!(v.ports, [None, Some(443), Some(8443)]);
    }

    #[test]
    fn an_address_that_could_not_be_admitted_is_not_handed_out() {
        let q = parse_query(&query_packet(1, "api.example.com", TYPE_A)).unwrap();
        let resolve = |_: &str| {
            vec![
                "93.184.216.34".parse().unwrap(),
                "93.184.216.35".parse().unwrap(),
            ]
        };
        let mut admit = |a: IpAddr, _: &[Option<u16>]| {
            if a.to_string().ends_with(".35") {
                Err(std::io::Error::other("nft said no"))
            } else {
                Ok(())
            }
        };
        let r = answer(
            &q,
            &rules(&["api.example.com:443"]),
            false,
            &resolve,
            &mut admit,
        );
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1, "only the admitted one");
    }

    #[test]
    fn private_addresses_are_dropped_from_answers_unless_allowed() {
        // DNS rebinding: an allowed name that resolves into the host's own
        // network. The filter would reject the connection anyway; the answer
        // simply does not offer it.
        let q = parse_query(&query_packet(1, "api.example.com", TYPE_A)).unwrap();
        let resolve = |_: &str| {
            vec![
                "10.0.0.5".parse().unwrap(),
                "93.184.216.34".parse().unwrap(),
            ]
        };
        let mut admit = |_: IpAddr, _: &[Option<u16>]| Ok(());
        let closed = answer(
            &q,
            &rules(&["api.example.com:443"]),
            false,
            &resolve,
            &mut admit,
        );
        assert_eq!(u16::from_be_bytes([closed[6], closed[7]]), 1);
        let open = answer(
            &q,
            &rules(&["api.example.com:443"]),
            true,
            &resolve,
            &mut admit,
        );
        assert_eq!(u16::from_be_bytes([open[6], open[7]]), 2);
    }

    #[test]
    fn other_record_types_get_an_empty_answer_not_a_refusal() {
        // A client asking for HTTPS or MX records must fall back to A/AAAA
        // rather than conclude the name does not exist.
        let q = parse_query(&query_packet(1, "api.example.com", 65)).unwrap();
        let resolve = |_: &str| vec!["93.184.216.34".parse().unwrap()];
        let mut admit = |_: IpAddr, _: &[Option<u16>]| Ok(());
        let r = answer(
            &q,
            &rules(&["api.example.com:443"]),
            false,
            &resolve,
            &mut admit,
        );
        assert_eq!(r[3] & 0x0F, 0);
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 0);
    }

    #[test]
    fn the_private_ranges_match_the_filters() {
        for p in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "127.0.0.53",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "0.1.2.3",
            "224.0.0.251",
            "239.255.255.250",
            "255.255.255.255",
            "ff02::fb",
        ] {
            assert!(is_private(p.parse().unwrap()), "{p}");
        }
        for p in ["1.1.1.1", "93.184.216.34", "100.128.0.1", "2606:4700::1111"] {
            assert!(!is_private(p.parse().unwrap()), "{p}");
        }
    }

    #[test]
    fn the_server_answers_over_a_real_socket_and_stops_when_told() {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let resolve = |_: &str| vec!["93.184.216.34".parse().unwrap()];
                let mut admit = |_: IpAddr, _: &[Option<u16>]| Ok(());
                serve(
                    socket,
                    rules(&["*.example.com:443"]),
                    false,
                    stop,
                    &resolve,
                    &mut admit,
                );
            })
        };

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        client
            .send_to(&query_packet(9, "api.example.com", TYPE_A), addr)
            .unwrap();
        let mut buf = [0u8; 512];
        let (n, _) = client.recv_from(&mut buf).expect("an answer");
        assert_eq!(&buf[0..2], &9u16.to_be_bytes());
        assert_eq!(u16::from_be_bytes([buf[6], buf[7]]), 1);
        assert_eq!(&buf[n - 4..n], &[93, 184, 216, 34]);

        client
            .send_to(&query_packet(10, "nope.example.net", TYPE_A), addr)
            .unwrap();
        let (_, _) = client.recv_from(&mut buf).expect("an answer");
        assert_eq!(buf[3] & 0x0F, Rcode::NxDomain as u8);

        stop.store(true, Ordering::Relaxed);
        server.join().expect("the server thread ends");
    }
}
