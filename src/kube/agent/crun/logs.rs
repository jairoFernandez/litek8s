//! Container logs in the CRI format the kubelet reads
//! (`<RFC 3339 nano time> <stdout|stderr> <F|P> <content>`, one record per
//! line; `P` marks a partial line continued by the next record). The shim
//! writes them; the agent turns them back into the stream `kubectl logs`
//! expects, with tail, since and timestamps applied.
// The writing half runs in the shim, which only exists on Linux.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io::Write;

/// Longest partial line kept before it is written out as a `P` record.
pub(crate) const MAX_LINE: usize = 16 * 1024;

/// Splits one stream's output into records.
#[derive(Default)]
pub(crate) struct LineSplitter {
    pending: Vec<u8>,
}

impl LineSplitter {
    /// Feed bytes; complete records are written to `out`.
    pub fn feed(&mut self, data: &[u8], stream: &str, now: &str, out: &mut impl Write) -> std::io::Result<()> {
        let mut rest = data;
        while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
            self.pending.extend_from_slice(&rest[..pos]);
            write_record(out, now, stream, true, &self.pending)?;
            self.pending.clear();
            rest = &rest[pos + 1..];
        }
        self.pending.extend_from_slice(rest);
        while self.pending.len() >= MAX_LINE {
            let chunk: Vec<u8> = self.pending.drain(..MAX_LINE).collect();
            write_record(out, now, stream, false, &chunk)?;
        }
        Ok(())
    }

    /// Write out what is left (the stream ended without a newline).
    pub fn finish(&mut self, stream: &str, now: &str, out: &mut impl Write) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            write_record(out, now, stream, false, &self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
}

fn write_record(out: &mut impl Write, now: &str, stream: &str, full: bool, content: &[u8]) -> std::io::Result<()> {
    let mut line = Vec::with_capacity(content.len() + 48);
    line.extend_from_slice(now.as_bytes());
    line.push(b' ');
    line.extend_from_slice(stream.as_bytes());
    line.extend_from_slice(if full { b" F " } else { b" P " });
    line.extend_from_slice(content);
    line.push(b'\n');
    out.write_all(&line)
}

/// The current time as the CRI log format writes it.
pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// One parsed record.
#[derive(Debug, PartialEq)]
pub(crate) struct Record<'a> {
    pub time: &'a str,
    pub stream: u8,
    pub full: bool,
    pub content: &'a [u8],
}

pub(crate) fn parse(line: &[u8]) -> Option<Record<'_>> {
    let mut parts = line.splitn(4, |b| *b == b' ');
    let time = std::str::from_utf8(parts.next()?).ok()?;
    let stream = match parts.next()? {
        b"stdout" => 1,
        b"stderr" => 2,
        _ => return None,
    };
    let full = match parts.next()? {
        b"F" => true,
        b"P" => false,
        _ => return None,
    };
    let content = parts.next().unwrap_or_default();
    Some(Record { time, stream, full, content })
}

/// What `kubectl logs` asked for.
#[derive(Debug, Clone, Default)]
pub(crate) struct Filter {
    pub timestamps: bool,
    /// Only records at or after this time (RFC 3339, compared as instants).
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

impl Filter {
    /// Render the records in `data` (whole lines only); returns the
    /// (stream, bytes) chunks to send.
    pub fn render(&self, data: &[u8], out: &mut Vec<(u8, Vec<u8>)>) {
        for line in data.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let Some(r) = parse(line) else { continue };
            if let Some(since) = self.since {
                match chrono::DateTime::parse_from_rfc3339(r.time) {
                    Ok(t) if t < since => continue,
                    _ => {}
                }
            }
            let mut bytes = Vec::with_capacity(r.content.len() + 40);
            if self.timestamps {
                bytes.extend_from_slice(r.time.as_bytes());
                bytes.push(b' ');
            }
            bytes.extend_from_slice(r.content);
            if r.full {
                bytes.push(b'\n');
            }
            match out.last_mut() {
                Some((stream, buf)) if *stream == r.stream && buf.len() < 32 * 1024 => buf.extend_from_slice(&bytes),
                _ => out.push((r.stream, bytes)),
            }
        }
    }
}

/// The byte offset in `data` where the last `n` full lines (of output, so a
/// run of `P` records counts once) start.
pub(crate) fn tail_offset(data: &[u8], n: usize) -> usize {
    if n == 0 {
        return data.len();
    }
    let mut seen = 0;
    let mut end = data.len();
    // Walk records backwards; a line ends at an `F` record.
    while end > 0 {
        let start = data[..end.saturating_sub(1)].iter().rposition(|b| *b == b'\n').map(|p| p + 1).unwrap_or(0);
        let line = &data[start..end];
        if parse(line.strip_suffix(b"\n").unwrap_or(line)).is_some_and(|r| r.full) {
            if seen == n {
                return end;
            }
            seen += 1;
        }
        end = start;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_and_partials() {
        let mut s = LineSplitter::default();
        let mut out = Vec::new();
        s.feed(b"hello\nwor", "stdout", "T1", &mut out).unwrap();
        s.feed(b"ld\n", "stdout", "T2", &mut out).unwrap();
        s.feed(b"tail", "stdout", "T3", &mut out).unwrap();
        s.finish("stdout", "T4", &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "T1 stdout F hello\nT2 stdout F world\nT4 stdout P tail\n");
    }

    #[test]
    fn long_lines_become_partial_records() {
        let mut s = LineSplitter::default();
        let mut out = Vec::new();
        let long = vec![b'x'; MAX_LINE + 10];
        s.feed(&long, "stderr", "T", &mut out).unwrap();
        s.feed(b"\n", "stderr", "T", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("T stderr P xxx"));
        assert_eq!(lines[1], format!("T stderr F {}", "x".repeat(10)));
    }

    #[test]
    fn renders_with_filters() {
        let data = b"2026-01-01T00:00:01.000000000Z stdout F one\n\
2026-01-01T00:00:02.000000000Z stderr F two\n\
2026-01-01T00:00:03.000000000Z stdout P thr\n\
2026-01-01T00:00:03.500000000Z stdout F ee\n";
        let mut out = Vec::new();
        Filter::default().render(data, &mut out);
        assert_eq!(out, vec![(1, b"one\n".to_vec()), (2, b"two\n".to_vec()), (1, b"three\n".to_vec())]);
        let mut out = Vec::new();
        let since = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:02Z").unwrap().with_timezone(&chrono::Utc);
        Filter { timestamps: true, since: Some(since) }.render(data, &mut out);
        assert_eq!(out[0], (2, b"2026-01-01T00:00:02.000000000Z two\n".to_vec()));
        let mut out = Vec::new();
        Filter::default().render(&data[tail_offset(data, 1)..], &mut out);
        assert_eq!(out, vec![(1, b"three\n".to_vec())]);
        assert_eq!(tail_offset(data, 5), 0);
        assert_eq!(tail_offset(data, 0), data.len());
    }
}
