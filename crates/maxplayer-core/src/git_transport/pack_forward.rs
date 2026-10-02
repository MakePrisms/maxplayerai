//! Buyer-owned staging packs only. Reuse the exact fetched bytes, without a
//! subprocess or libgit2's uncancellable delta search. All wire legs share the
//! existing transport's binding, authentication, timeout and redirect policy.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Cursor;

const RESPONSE_LIMIT: u64 = 1024 * 1024;

pub(super) struct Advertisement {
    pub refs: BTreeMap<String, Oid>,
    capabilities: BTreeSet<String>,
}
impl Advertisement {
    pub fn can_forward(&self) -> bool {
        self.refs.is_empty()
            && self.capabilities.contains("report-status")
            && self.capabilities.contains("ofs-delta")
    }
}
fn invalid(message: &str) -> TransportError {
    TransportError::Transport(format!("private input pack: {message}"))
}
fn io_error(error: impl std::fmt::Display) -> TransportError {
    TransportError::Io(error.to_string())
}
fn packets(bytes: &[u8]) -> Result<Vec<&[u8]>, TransportError> {
    if !bytes.ends_with(b"0000") {
        return Err(invalid("missing final flush packet"));
    }
    let mut remaining = bytes;
    let mut lines = Vec::new();
    while !remaining.is_empty() {
        if remaining.len() < 4 {
            return Err(invalid("truncated pkt-line"));
        }
        let len = std::str::from_utf8(&remaining[..4])
            .ok()
            .and_then(|s| usize::from_str_radix(s, 16).ok())
            .ok_or_else(|| invalid("invalid pkt-line length"))?;
        if len == 0 {
            remaining = &remaining[4..];
            continue;
        }
        if !(4..=65520).contains(&len) || len > remaining.len() {
            return Err(invalid("invalid pkt-line size"));
        }
        lines.push(&remaining[4..len]);
        remaining = &remaining[len..];
    }
    Ok(lines)
}
fn parse_advertisement(bytes: &[u8]) -> Result<Advertisement, TransportError> {
    let mut lines = packets(bytes)?.into_iter();
    if lines.next() != Some(b"# service=git-receive-pack\n".as_slice()) {
        return Err(invalid("missing receive-pack advertisement"));
    }
    let mut result = Advertisement {
        refs: BTreeMap::new(),
        capabilities: BTreeSet::new(),
    };
    for (index, line) in lines.enumerate() {
        let line = std::str::from_utf8(line)
            .map_err(|_| invalid("non-UTF8 ref"))?
            .trim_end_matches('\n');
        let (reference, capabilities) = line
            .split_once('\0')
            .map_or((line, None), |(r, c)| (r, Some(c)));
        if let Some(caps) = capabilities {
            if index != 0 {
                return Err(invalid("capabilities after first ref"));
            }
            result
                .capabilities
                .extend(caps.split_whitespace().map(str::to_owned));
        }
        let (oid, name) = reference
            .split_once(' ')
            .ok_or_else(|| invalid("malformed advertised ref"))?;
        let oid = Oid::from_str(oid).map_err(|_| invalid("invalid advertised oid"))?;
        if oid.is_zero() && name == "capabilities^{}" {
            continue;
        }
        if result.refs.insert(name.to_owned(), oid).is_some() {
            return Err(invalid("duplicate advertised ref"));
        }
    }
    Ok(result)
}
fn stream(url: &str, mint: AuthMinter, service: Service) -> Result<HttpStream, TransportError> {
    assert_allowed_repo_locator(url)?;
    Ok(NostrHttp {
        mint: Some(mint),
        authority: None,
        lifetime: None,
        short: false,
        read_budget: None,
        intended_url: Some(url.to_owned()),
    }
    .stream(url, service)
    .map_err(map_git_error)?)
}
fn response(mut stream: HttpStream) -> Result<Vec<u8>, TransportError> {
    stream
        .send()
        .map_err(|e| map_git_error(git2::Error::from_str(&e.to_string())))?;
    let mut bytes = Vec::new();
    stream
        .response
        .take()
        .ok_or_else(|| invalid("missing response"))?
        .take(RESPONSE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > RESPONSE_LIMIT {
        return Err(invalid("response exceeds limit"));
    }
    Ok(bytes)
}
pub(super) fn advertise(url: &str, mint: AuthMinter) -> Result<Advertisement, TransportError> {
    parse_advertisement(&response(stream(url, mint, Service::ReceivePackLs)?)?)
}

/// No alternates, loose objects, symlinks or multiple packs. The staging repo is
/// owned by the buyer and already passed check_objects; the relay still verifies
/// connectivity and indexes the forwarded pack before accepting the ref.
pub(super) fn candidate(repo: &Repository) -> Result<Option<File>, TransportError> {
    if !repo.is_bare() {
        return Ok(None);
    }
    let objects = repo.path().join("objects");
    if objects.join("info/alternates").exists() || objects.join("info/http-alternates").exists() {
        return Ok(None);
    }
    let mut pack = None;
    for entry in std::fs::read_dir(&objects).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        let kind = entry.file_type().map_err(io_error)?;
        if kind.is_symlink() || !kind.is_dir() {
            return Ok(None);
        }
        if name == "info" {
            continue;
        }
        if name != "pack" {
            if std::fs::read_dir(entry.path())
                .map_err(io_error)?
                .next()
                .is_some()
            {
                return Ok(None);
            }
            continue;
        }
        for item in std::fs::read_dir(entry.path()).map_err(io_error)? {
            let item = item.map_err(io_error)?;
            if !item.file_type().map_err(io_error)?.is_file() {
                return Ok(None);
            }
            if item.path().extension().is_some_and(|e| e == "pack") {
                if pack.is_some() {
                    return Ok(None);
                }
                pack = Some(item.path());
            }
        }
    }
    let Some(path) = pack else {
        return Ok(None);
    };
    let mut file = File::open(path).map_err(io_error)?;
    let len = file.metadata().map_err(io_error)?.len();
    if !(32..=crate::private_content::MAX_GIT_TRANSFER_BYTES as u64).contains(&len) {
        return Ok(None);
    }
    let mut magic = [0; 4];
    file.read_exact(&mut magic).map_err(io_error)?;
    if &magic != b"PACK" {
        return Ok(None);
    }
    use std::io::Seek;
    file.rewind().map_err(io_error)?;
    Ok(Some(file))
}
fn command(reference: &str, oid: Oid) -> Vec<u8> {
    let line = format!(
        "{} {oid} {reference}\0report-status ofs-delta\n",
        Oid::zero()
    );
    format!("{:04x}{line}0000", line.len() + 4).into_bytes()
}
fn status(bytes: &[u8], reference: &str) -> Result<(), TransportError> {
    let lines = packets(bytes)?;
    let expected = format!("ok {reference}\n");
    if lines.as_slice() != [b"unpack ok\n".as_slice(), expected.as_bytes()] {
        return Err(TransportError::Rejected(
            "private input push did not acknowledge unpack and exactly the intended ref".into(),
        ));
    }
    Ok(())
}
pub(super) fn push(
    url: &str,
    reference: &str,
    oid: Oid,
    mint: AuthMinter,
    file: File,
) -> Result<String, TransportError> {
    let prefix = command(reference, oid);
    let length = prefix.len() as u64 + file.metadata().map_err(io_error)?.len();
    let mut leg = stream(url, mint, Service::ReceivePack)?;
    leg.streaming_body = Some(reqwest::blocking::Body::sized(
        Cursor::new(prefix).chain(file),
        length,
    ));
    status(&response(leg)?, reference)?;
    crate::opline!("buyer private input: forwarded fetched pack without rebuilding deltas");
    Ok(oid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pkt(s: &str) -> String {
        format!("{:04x}{s}", s.len() + 4)
    }
    #[test]
    fn forward_status_requires_exact_success() {
        for body in [
            "0000".to_owned(),
            pkt("unpack ok\n"),
            format!("{}{}0000", pkt("unpack ok\n"), pkt("ok refs/heads/wrong\n")),
            format!(
                "{}{}0000",
                pkt("unpack ok\n"),
                pkt("ng refs/heads/input/test rejected\n")
            ),
            format!(
                "{}{}0000",
                pkt("unpack bad pack\n"),
                pkt("ok refs/heads/input/test\n")
            ),
        ] {
            assert!(status(body.as_bytes(), "refs/heads/input/test").is_err());
        }
        assert!(
            status(
                format!(
                    "{}{}0000",
                    pkt("unpack ok\n"),
                    pkt("ok refs/heads/input/test\n")
                )
                .as_bytes(),
                "refs/heads/input/test"
            )
            .is_ok()
        );
    }
    #[test]
    fn forward_requires_empty_advertisement_and_both_capabilities() {
        for (caps, empty, expected) in [
            ("report-status ofs-delta", true, true),
            ("report-status", true, false),
            ("ofs-delta", true, false),
            ("report-status ofs-delta", false, false),
        ] {
            let oid = if empty {
                Oid::zero()
            } else {
                Oid::from_str(&"ab".repeat(20)).unwrap()
            };
            let name = if empty {
                "capabilities^{}"
            } else {
                "refs/heads/main"
            };
            let data = format!(
                "{}0000{}0000",
                pkt("# service=git-receive-pack\n"),
                pkt(&format!("{oid} {name}\0{caps}\n"))
            );
            assert_eq!(
                parse_advertisement(data.as_bytes()).unwrap().can_forward(),
                expected
            );
        }
    }
    #[test]
    fn forward_candidate_rejects_oversize_loose_and_multiple_packs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(dir.path()).unwrap();
        let pack = repo.path().join("objects/pack/pack-fixture.pack");
        std::fs::write(&pack, [b"PACK".as_slice(), &[0; 28]].concat()).unwrap();
        assert!(candidate(&repo).unwrap().is_some());
        let extra = pack.with_file_name("pack-other.pack");
        std::fs::copy(&pack, &extra).unwrap();
        assert!(candidate(&repo).unwrap().is_none());
        std::fs::remove_file(extra).unwrap();
        let loose = repo.path().join("objects/aa");
        std::fs::create_dir(&loose).unwrap();
        std::fs::write(loose.join("object"), b"loose").unwrap();
        assert!(candidate(&repo).unwrap().is_none());
        std::fs::remove_dir_all(loose).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(pack)
            .unwrap()
            .set_len(crate::private_content::MAX_GIT_TRANSFER_BYTES as u64 + 1)
            .unwrap();
        assert!(candidate(&repo).unwrap().is_none());
    }
}
