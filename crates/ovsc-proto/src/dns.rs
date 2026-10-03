//! Minimal DNS message codec for multicast DNS (RFC 1035 / RFC 6762).
//!
//! Only what Dante discovery needs: questions and A, PTR, SRV and TXT
//! records, with name compression on both encode and decode. Labels are kept
//! as raw strings so that DNS-SD instance names such as `01@Stage Box` (which
//! contain `@` and spaces) survive untouched.

use std::collections::HashMap;
use std::fmt;
use std::net::Ipv4Addr;

use crate::wire::{Reader, Writer};
use crate::{Error, Result};

/// Record types.
pub mod rtype {
    pub const A: u16 = 1;
    pub const PTR: u16 = 12;
    pub const TXT: u16 = 16;
    pub const AAAA: u16 = 28;
    pub const SRV: u16 = 33;
    pub const NSEC: u16 = 47;
    pub const ANY: u16 = 255;
}

pub const CLASS_IN: u16 = 1;
/// Top bit of a record's class: "cache flush" (RFC 6762 §10.2).
const CACHE_FLUSH: u16 = 0x8000;
/// Top bit of a question's class: "unicast response requested".
const UNICAST_RESPONSE: u16 = 0x8000;
/// Header flags of an authoritative response.
pub const FLAGS_RESPONSE: u16 = 0x8400;

/// A domain name as a list of labels.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Name(pub Vec<String>);

impl Name {
    /// Builds a name from labels.
    pub fn from_labels<S: Into<String>>(labels: impl IntoIterator<Item = S>) -> Self {
        Self(labels.into_iter().map(Into::into).collect())
    }

    /// Parses a dotted name. Use [`Name::instance`] for names whose first
    /// label may contain dots.
    pub fn parse(s: &str) -> Self {
        Self(s.trim_end_matches('.').split('.').filter(|l| !l.is_empty()).map(Into::into).collect())
    }

    /// `<instance>.<service type>`; `instance` is kept as one label.
    pub fn instance(instance: &str, service: &Name) -> Self {
        let mut labels = vec![instance.to_owned()];
        labels.extend(service.0.iter().cloned());
        Self(labels)
    }

    /// Case-insensitive comparison, as DNS requires.
    pub fn eq_ignore_case(&self, other: &Name) -> bool {
        self.0.len() == other.0.len()
            && self.0.iter().zip(&other.0).all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    /// Whether `self` is `<one label>.<suffix>`.
    pub fn is_direct_child_of(&self, suffix: &Name) -> bool {
        self.0.len() == suffix.0.len() + 1
            && self.0[1..].iter().zip(&suffix.0).all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    pub fn first_label(&self) -> Option<&str> {
        self.0.first().map(String::as_str)
    }

    /// Compression key. Case-sensitive on purpose, so that names keep the
    /// exact spelling they were advertised with.
    fn key(labels: &[String]) -> String {
        labels.join("\0")
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.", self.0.join("."))
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub name: Name,
    pub qtype: u16,
    pub unicast_response: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RData {
    A(Ipv4Addr),
    Ptr(Name),
    Srv { priority: u16, weight: u16, port: u16, target: Name },
    Txt(Vec<Vec<u8>>),
    Other(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub name: Name,
    pub rtype: u16,
    pub cache_flush: bool,
    pub ttl: u32,
    pub data: RData,
}

impl Record {
    pub fn a(name: Name, ttl: u32, addr: Ipv4Addr) -> Self {
        Self { name, rtype: rtype::A, cache_flush: true, ttl, data: RData::A(addr) }
    }

    pub fn ptr(name: Name, ttl: u32, target: Name) -> Self {
        Self { name, rtype: rtype::PTR, cache_flush: false, ttl, data: RData::Ptr(target) }
    }

    pub fn srv(name: Name, ttl: u32, port: u16, target: Name) -> Self {
        let data = RData::Srv { priority: 0, weight: 0, port, target };
        Self { name, rtype: rtype::SRV, cache_flush: true, ttl, data }
    }

    pub fn txt(name: Name, ttl: u32, entries: Vec<Vec<u8>>) -> Self {
        Self { name, rtype: rtype::TXT, cache_flush: true, ttl, data: RData::Txt(entries) }
    }
}

/// A DNS message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub id: u16,
    pub flags: u16,
    pub questions: Vec<Question>,
    pub answers: Vec<Record>,
    pub authorities: Vec<Record>,
    pub additionals: Vec<Record>,
}

impl Message {
    pub fn query(questions: Vec<Question>) -> Self {
        Self { questions, ..Default::default() }
    }

    pub fn response() -> Self {
        Self { flags: FLAGS_RESPONSE, ..Default::default() }
    }

    pub fn is_response(&self) -> bool {
        self.flags & 0x8000 != 0
    }

    /// All records of the message (answers, authorities, additionals).
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.answers.iter().chain(&self.authorities).chain(&self.additionals)
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut r = Reader::new(buf);
        let id = r.u16()?;
        let flags = r.u16()?;
        let counts = [r.u16()?, r.u16()?, r.u16()?, r.u16()?];
        let mut msg = Message { id, flags, ..Default::default() };
        for _ in 0..counts[0] {
            let name = read_name(buf, &mut r)?;
            let qtype = r.u16()?;
            let qclass = r.u16()?;
            msg.questions.push(Question {
                name,
                qtype,
                unicast_response: qclass & UNICAST_RESPONSE != 0,
            });
        }
        for (count, list) in [
            (counts[1], &mut msg.answers),
            (counts[2], &mut msg.authorities),
            (counts[3], &mut msg.additionals),
        ] {
            for _ in 0..count {
                list.push(read_record(buf, &mut r)?);
            }
        }
        Ok(msg)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut enc = Encoder { w: Writer::with_capacity(512), names: HashMap::new() };
        enc.w.u16(self.id);
        enc.w.u16(self.flags);
        enc.w.u16(self.questions.len() as u16);
        enc.w.u16(self.answers.len() as u16);
        enc.w.u16(self.authorities.len() as u16);
        enc.w.u16(self.additionals.len() as u16);
        for q in &self.questions {
            enc.name(&q.name);
            enc.w.u16(q.qtype);
            enc.w.u16(CLASS_IN | if q.unicast_response { UNICAST_RESPONSE } else { 0 });
        }
        for rec in self.answers.iter().chain(&self.authorities).chain(&self.additionals) {
            enc.record(rec);
        }
        enc.w.into_vec()
    }
}

fn read_name(buf: &[u8], r: &mut Reader<'_>) -> Result<Name> {
    let mut labels = Vec::new();
    let mut pos = r.pos();
    let mut jumped = false;
    let mut jumps = 0;
    loop {
        let len = *buf.get(pos).ok_or(Error::Invalid("dns name runs past end"))? as usize;
        match len & 0xc0 {
            0x00 if len == 0 => {
                pos += 1;
                break;
            }
            0x00 => {
                let label = buf
                    .get(pos + 1..pos + 1 + len)
                    .ok_or(Error::Invalid("dns label runs past end"))?;
                labels.push(String::from_utf8_lossy(label).into_owned());
                pos += 1 + len;
            }
            0xc0 => {
                let lo = *buf.get(pos + 1).ok_or(Error::Invalid("dns pointer truncated"))?;
                if !jumped {
                    r.skip(pos + 2 - r.pos())?;
                    jumped = true;
                }
                jumps += 1;
                if jumps > 64 {
                    return Err(Error::Invalid("dns compression loop"));
                }
                pos = ((len & 0x3f) << 8) | lo as usize;
            }
            _ => return Err(Error::Invalid("dns label type")),
        }
    }
    if !jumped {
        r.skip(pos - r.pos())?;
    }
    Ok(Name(labels))
}

fn read_record(buf: &[u8], r: &mut Reader<'_>) -> Result<Record> {
    let name = read_name(buf, r)?;
    let rtype = r.u16()?;
    let class = r.u16()?;
    let ttl = r.u32()?;
    let rdlen = r.u16()? as usize;
    let start = r.pos();
    let rdata = r.bytes(rdlen)?;
    let data = match rtype {
        rtype::A if rdlen == 4 => RData::A(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3])),
        rtype::PTR => RData::Ptr(read_name(buf, &mut Reader::at(buf, start))?),
        rtype::SRV => {
            let mut sr = Reader::at(buf, start);
            let priority = sr.u16()?;
            let weight = sr.u16()?;
            let port = sr.u16()?;
            RData::Srv { priority, weight, port, target: read_name(buf, &mut sr)? }
        }
        rtype::TXT => {
            let mut entries = Vec::new();
            let mut tr = Reader::new(rdata);
            while tr.remaining() > 0 {
                let n = tr.u8()? as usize;
                entries.push(tr.bytes(n)?.to_vec());
            }
            RData::Txt(entries)
        }
        _ => RData::Other(rdata.to_vec()),
    };
    Ok(Record { name, rtype, cache_flush: class & CACHE_FLUSH != 0, ttl, data })
}

struct Encoder {
    w: Writer,
    names: HashMap<String, u16>,
}

impl Encoder {
    fn name(&mut self, name: &Name) {
        let labels = &name.0;
        for i in 0..labels.len() {
            let key = Name::key(&labels[i..]);
            if let Some(&at) = self.names.get(&key) {
                self.w.u16(0xc000 | at);
                return;
            }
            let at = self.w.len();
            if at < 0x3fff {
                self.names.insert(key, at as u16);
            }
            let bytes = labels[i].as_bytes();
            let n = bytes.len().min(63);
            self.w.u8(n as u8);
            self.w.bytes(&bytes[..n]);
        }
        self.w.u8(0);
    }

    fn record(&mut self, rec: &Record) {
        self.name(&rec.name);
        self.w.u16(rec.rtype);
        self.w.u16(CLASS_IN | if rec.cache_flush { CACHE_FLUSH } else { 0 });
        self.w.u32(rec.ttl);
        let len_at = self.w.u16(0) as usize;
        match &rec.data {
            RData::A(addr) => {
                self.w.bytes(&addr.octets());
            }
            RData::Ptr(target) => self.name(target),
            RData::Srv { priority, weight, port, target } => {
                self.w.u16(*priority);
                self.w.u16(*weight);
                self.w.u16(*port);
                self.name(target);
            }
            RData::Txt(entries) => {
                if entries.is_empty() {
                    self.w.u8(0);
                }
                for e in entries {
                    let n = e.len().min(255);
                    self.w.u8(n as u8);
                    self.w.bytes(&e[..n]);
                }
            }
            RData::Other(bytes) => {
                self.w.bytes(bytes);
            }
        }
        let rdlen = self.w.len() - len_at - 2;
        self.w.patch_u16(len_at, rdlen as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chan_service() -> Name {
        Name::parse("_netaudio-chan._udp.local")
    }

    #[test]
    fn names_with_special_characters_round_trip() {
        let inst = Name::instance("01@Stage Box.2", &chan_service());
        assert_eq!(inst.0.len(), 4);
        assert!(inst.is_direct_child_of(&chan_service()));
        assert_eq!(inst.first_label(), Some("01@Stage Box.2"));

        let mut msg = Message::response();
        msg.answers.push(Record::ptr(chan_service(), 4500, inst.clone()));
        msg.answers.push(Record::srv(inst.clone(), 120, 4455, Name::parse("Stage-Box.local")));
        msg.answers.push(Record::txt(
            inst.clone(),
            4500,
            vec![b"id=1".to_vec(), b"default".to_vec()],
        ));
        msg.additionals.push(Record::a(
            Name::parse("stage-box.local"),
            120,
            Ipv4Addr::new(10, 1, 2, 3),
        ));
        let bytes = msg.encode();
        let decoded = Message::decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
        assert!(decoded.is_response());
    }

    #[test]
    fn compression_shrinks_repeated_suffixes() {
        let mut msg = Message::response();
        for i in 0..8 {
            msg.answers.push(Record::ptr(
                chan_service(),
                4500,
                Name::instance(&format!("{i:02}@dev"), &chan_service()),
            ));
        }
        let bytes = msg.encode();
        // Each repeated answer costs only a pointer for its owner name plus
        // one label and a pointer for the target.
        assert!(bytes.len() < 12 + 40 + 8 * 30, "{} bytes", bytes.len());
        assert_eq!(Message::decode(&bytes).unwrap(), msg);
    }

    #[test]
    fn query_flags() {
        let q = Message::query(vec![Question {
            name: Name::parse("x._netaudio-arc._udp.local"),
            qtype: rtype::SRV,
            unicast_response: true,
        }]);
        let decoded = Message::decode(&q.encode()).unwrap();
        assert!(!decoded.is_response());
        assert!(decoded.questions[0].unicast_response);
    }

    #[test]
    fn rejects_compression_loops_and_garbage() {
        // Header with one question whose name points at itself.
        let mut buf = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        buf.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1]);
        assert!(Message::decode(&buf).is_err());
        assert!(Message::decode(&[1, 2, 3]).is_err());
    }

    #[test]
    fn name_equality_ignores_case() {
        assert!(Name::parse("Dev.local").eq_ignore_case(&Name::parse("dev.LOCAL")));
        assert!(!Name::parse("dev.local").eq_ignore_case(&Name::parse("dev2.local")));
    }
}
