use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{self, Read as _, Seek as _, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dudes_proto::{MailAct, MailFrom, MailTo};
use dudes_rpc::conditioned::payload_digest;
use dudes_types::{AgentId, Escaped, Lsn, MailId, ThreadId};
use rustix::fs::{AtFlags, Dir, Mode, OFlags, RenameFlags};
use serde::{Deserialize, Serialize};

use crate::args::{MailAddress, MailCommand, MailRoute, MailSend};

const MARKER_BYTES: u64 = 65_536;
const BODY_BYTES: u64 = 1_048_576;
const INLINE_BYTES: u64 = 4_096;
const SCAN_CAP: usize = 1_024;
const INBOX_CAP: usize = 32;
/// Entries the outbox may already hold before a new acknowledgement request is refused, so the
/// daemon's bounded outbox scan always sees every request.
const CONTROL_OUTBOX_CAP: usize = 64;
const REPLY_PREFIX: &str = "Re: ";
const CONTROL_SUFFIX: &str = ".control.json";

#[derive(Serialize)]
struct Request<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<&'a MailAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    in_reply_to: Option<MailId>,
    act: MailAct,
    subject: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
}

/// A request a scoped agent can only make through its own outbox. The daemon decodes it strictly.
#[derive(Serialize)]
#[serde(tag = "request", rename_all = "snake_case")]
enum Control {
    MailDone { marker: MailId },
    ClosingAck { announcement: Lsn },
}

#[derive(Deserialize)]
struct Marker {
    #[serde(deserialize_with = "canonical_id")]
    id: MailId,
    from: MailFrom,
    to: MailTo,
    act: MailAct,
    subject: String,
    body: Option<String>,
    body_len: u64,
    body_path: Option<PathBuf>,
    body_blake3: Option<String>,
    #[serde(default)]
    thread: Option<ThreadId>,
    #[serde(default)]
    thread_seq: u32,
    #[serde(default)]
    members: Vec<AgentId>,
}

fn canonical_id<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<MailId, D::Error> {
    let text = String::deserialize(decoder)?;
    text.parse::<MailId>()
        .ok()
        .filter(|id| id.to_string() == text)
        .ok_or_else(|| serde::de::Error::custom("noncanonical mail id"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn rename(source: &File, from: &str, destination: &File, to: &str) -> io::Result<()> {
    rustix::fs::renameat_with(source, from, destination, to, RenameFlags::NOREPLACE)
        .map_err(Into::into)
}

fn open(parent: impl AsFd, name: impl rustix::path::Arg, flags: OFlags) -> io::Result<File> {
    rustix::fs::openat(
        parent,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)
    .map_err(Into::into)
}

fn same(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.file_type() == right.file_type()
}

fn names(directory: &File) -> io::Result<Vec<OsString>> {
    let mut listing = Dir::read_from(directory)?;
    let mut names = Vec::new();
    while let Some(entry) = listing.read() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() == SCAN_CAP {
            return Err(invalid("mailbox directory exceeds its scan cap"));
        }
        names.push(OsStr::from_bytes(name).to_owned());
    }
    names.sort();
    Ok(names)
}

fn present(directory: &File, name: &OsStr) -> io::Result<bool> {
    let names = names(directory)?;
    if names
        .iter()
        .any(|found| found != name && found.as_bytes().eq_ignore_ascii_case(name.as_bytes()))
    {
        return Err(invalid("noncanonical mailbox filename"));
    }
    Ok(names.iter().any(|found| found == name))
}

fn named(directory: &File, name: &OsStr) -> io::Result<()> {
    if present(directory, name)? {
        Ok(())
    } else {
        Err(io::ErrorKind::NotFound.into())
    }
}

fn validate_file(directory: &File, name: &OsStr, file: &File) -> io::Result<()> {
    named(directory, name)?;
    let actual = file.metadata()?;
    let current = open(directory, name, OFlags::RDONLY)?.metadata()?;
    if !actual.is_file() || actual.nlink() != 1 || current.nlink() != 1 || !same(&actual, &current)
    {
        return Err(invalid("mailbox file identity changed or is ambiguous"));
    }
    Ok(())
}

fn read_file(directory: &File, name: &OsStr, cap: u64) -> io::Result<(File, Vec<u8>)> {
    named(directory, name)?;
    let file = open(directory, name, OFlags::RDONLY)?;
    let before = file.metadata()?;
    if !before.is_file() || before.nlink() != 1 || before.len() > cap {
        return Err(invalid("mailbox file identity or length is ambiguous"));
    }
    validate_file(directory, name, &file)?;
    let mut bytes = Vec::new();
    (&file).take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(invalid("mailbox file exceeds its byte cap"));
    }
    validate_file(directory, name, &file)?;
    Ok((file, bytes))
}

fn verify_handoff(directory: &File, name: &str, file: &File, expected: &[u8]) -> io::Result<()> {
    let before = file.metadata()?;
    if !before.is_file() || before.nlink() > 1 || before.len() != expected.len() as u64 {
        return Err(invalid("mailbox handoff identity or bytes changed"));
    }
    let mut retained = file;
    retained.rewind()?;
    let mut bytes = Vec::new();
    retained
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes != expected || !same(&before, &after) || after.nlink() > 1 {
        return Err(invalid("mailbox handoff identity or bytes changed"));
    }
    match read_file(directory, OsStr::new(name), expected.len() as u64) {
        Ok((named, bytes)) => {
            if !same(&after, &named.metadata()?) || bytes != expected {
                return Err(invalid("mailbox handoff identity or bytes changed"));
            }
            Ok(())
        }
        Err(error) => {
            if !present(directory, OsStr::new(name))? && file.metadata()?.nlink() == 0 {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

struct Mailbox {
    parent_path: PathBuf,
    parent: File,
    name: OsString,
    directory: File,
    id: AgentId,
}

impl Mailbox {
    fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid("DUDES_MAILBOX must be an absolute agent directory"));
        }
        let name = path
            .file_name()
            .ok_or_else(|| invalid("mailbox has no agent directory name"))?;
        let id = name
            .to_str()
            .and_then(|text| text.parse::<AgentId>().ok())
            .filter(|id| name == OsStr::new(&id.to_string()))
            .ok_or_else(|| invalid("mailbox directory is not a canonical agent id"))?;
        let parent_path = path
            .parent()
            .ok_or_else(|| invalid("mailbox has no parent"))?
            .to_path_buf();
        let parent = open(
            rustix::fs::CWD,
            &parent_path,
            OFlags::RDONLY | OFlags::DIRECTORY,
        )?;
        named(&parent, name)?;
        let directory = open(&parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let mailbox = Self {
            parent_path,
            parent,
            name: name.to_owned(),
            directory,
            id,
        };
        mailbox.validate()?;
        Ok(mailbox)
    }

    fn validate(&self) -> io::Result<()> {
        let parent = open(
            rustix::fs::CWD,
            &self.parent_path,
            OFlags::RDONLY | OFlags::DIRECTORY,
        )?;
        named(&parent, &self.name)?;
        let directory = open(&parent, &self.name, OFlags::RDONLY | OFlags::DIRECTORY)?;
        if !same(&parent.metadata()?, &self.parent.metadata()?)
            || !same(&directory.metadata()?, &self.directory.metadata()?)
        {
            return Err(invalid("mailbox directory identity changed"));
        }
        Ok(())
    }

    fn child(&self, name: &str) -> io::Result<File> {
        self.validate()?;
        named(&self.directory, OsStr::new(name))?;
        open(&self.directory, name, OFlags::RDONLY | OFlags::DIRECTORY)
    }

    fn validate_child(&self, name: &str, directory: &File) -> io::Result<()> {
        let current = self.child(name)?;
        if !same(&current.metadata()?, &directory.metadata()?) {
            return Err(invalid("mailbox child directory identity changed"));
        }
        Ok(())
    }

    fn body(&self, inbox: &File, marker: &Marker) -> io::Result<Option<String>> {
        match (
            &marker.body,
            marker.body_len,
            &marker.body_path,
            &marker.body_blake3,
        ) {
            (None, 0, None, None) => Ok(None),
            (Some(body), len, None, None) if len <= INLINE_BYTES && body.len() as u64 == len => {
                Ok(Some(body.clone()))
            }
            (None, len, Some(path), Some(hash)) if len > INLINE_BYTES && len <= BODY_BYTES => {
                let name = format!("{}.body", marker.id);
                let expected = PathBuf::from("agents")
                    .join(self.id.to_string())
                    .join("inbox")
                    .join(&name);
                if path.as_os_str() != expected.as_os_str() {
                    return Err(invalid("noncanonical mail body reference"));
                }
                let (_, bytes) = read_file(inbox, OsStr::new(&name), BODY_BYTES)?;
                let observed = payload_digest(&bytes);
                if observed.decoded_len != len || &observed.blake3 != hash {
                    return Err(invalid("mail body length or digest differs"));
                }
                String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_| invalid("mail body is not UTF-8"))
            }
            _ => Err(invalid("mail body fields are inconsistent")),
        }
    }

    fn marker(
        &self,
        inbox: &File,
        id: MailId,
    ) -> io::Result<(File, Vec<u8>, Marker, Option<String>)> {
        let name = format!("{id}.json");
        let (file, bytes) = read_file(inbox, OsStr::new(&name), MARKER_BYTES)?;
        let marker = self.decode_marker(id, &bytes)?;
        let body = self.body(inbox, &marker)?;
        self.validate_child("inbox", inbox)?;
        validate_file(inbox, OsStr::new(&name), &file)?;
        Ok((file, bytes, marker, body))
    }

    fn decode_marker(&self, id: MailId, bytes: &[u8]) -> io::Result<Marker> {
        let marker: Marker =
            serde_json::from_slice(bytes).map_err(|_| invalid("malformed mail marker"))?;
        if marker.id != id || marker.to != (MailTo::Agent { agent: self.id }) {
            return Err(invalid("mail marker id or recipient differs"));
        }
        Ok(marker)
    }

    fn reply_subject(&self, id: MailId) -> io::Result<String> {
        let inbox = self.child("inbox")?;
        let name = format!("{id}.json");
        let (file, bytes, done) = match read_file(&inbox, OsStr::new(&name), MARKER_BYTES) {
            Ok((file, bytes)) => (file, bytes, None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let done = open(&inbox, ".done", OFlags::RDONLY | OFlags::DIRECTORY)?;
                let (file, bytes) = read_file(&done, OsStr::new(&name), MARKER_BYTES)?;
                (file, bytes, Some(done))
            }
            Err(error) => return Err(error),
        };
        let marker = self.decode_marker(id, &bytes)?;
        self.validate_child("inbox", &inbox)?;
        if let Some(done) = done {
            let current = open(&inbox, ".done", OFlags::RDONLY | OFlags::DIRECTORY)?;
            if !same(&done.metadata()?, &current.metadata()?) {
                return Err(invalid("acknowledgement directory changed"));
            }
            validate_file(&done, OsStr::new(&name), &file)?;
        } else {
            validate_file(&inbox, OsStr::new(&name), &file)?;
        }
        Ok(format!("{REPLY_PREFIX}{}", marker.subject))
    }

    fn request(&self, send: &MailSend) -> io::Result<Vec<u8>> {
        let (to, in_reply_to) = match &send.route {
            MailRoute::To(address) => (Some(address), None),
            MailRoute::Reply(id) => (None, Some(*id)),
        };
        let mut subject = match (&send.subject, &send.route) {
            (Some(subject), _) => Cow::Borrowed(subject.as_str()),
            (None, MailRoute::Reply(id)) => {
                let subject = self.reply_subject(*id).map_err(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        io::Error::new(
                            error.kind(),
                            "reply parent is unavailable locally; provide --subject explicitly",
                        )
                    } else {
                        error
                    }
                })?;
                Cow::Owned(subject)
            }
            (None, MailRoute::To(_)) => return Err(invalid("new mail requires a subject")),
        };
        let encode = |subject: &str| {
            serde_json::to_vec(&Request {
                to,
                in_reply_to,
                act: send.act,
                subject,
                body: send.body.as_deref(),
            })
        };
        let mut bytes = encode(&subject)?;
        if bytes.len() as u64 > BODY_BYTES && send.subject.is_none() {
            let excess = bytes.len() - usize::try_from(BODY_BYTES).unwrap();
            let mut cut = subject.len().saturating_sub(excess).max(REPLY_PREFIX.len());
            while !subject.is_char_boundary(cut) {
                cut -= 1;
            }
            subject.to_mut().truncate(cut);
            bytes = encode(&subject)?;
        }
        if bytes.len() as u64 > BODY_BYTES {
            return Err(invalid("outgoing mail exceeds its byte cap"));
        }
        Ok(bytes)
    }

    fn read(&self) -> io::Result<Vec<(Marker, Option<String>)>> {
        let inbox = self.child("inbox")?;
        let mut result = Vec::new();
        for name in names(&inbox)? {
            if Path::new(&name).extension() != Some(OsStr::new("json")) {
                continue;
            }
            let id = name
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|text| text.parse::<MailId>().ok())
                .filter(|id| name == OsStr::new(&format!("{id}.json")))
                .ok_or_else(|| invalid("noncanonical inbox marker filename"))?;
            if result.len() == INBOX_CAP {
                return Err(invalid("inbox exceeds its marker cap"));
            }
            let (_, _, marker, body) = self.marker(&inbox, id)?;
            result.push((marker, body));
        }
        self.validate_child("inbox", &inbox)?;
        Ok(result)
    }

    fn done(&self, id: MailId) -> io::Result<()> {
        self.done_with(id, rename)
    }

    fn done_with(
        &self,
        id: MailId,
        rename: impl FnOnce(&File, &str, &File, &str) -> io::Result<()>,
    ) -> io::Result<()> {
        let inbox = self.child("inbox")?;
        named(&inbox, OsStr::new(".done"))?;
        let done = open(&inbox, ".done", OFlags::RDONLY | OFlags::DIRECTORY)?;
        let (file, bytes, _, _) = self.marker(&inbox, id)?;
        let name = format!("{id}.json");
        let (current, current_bytes) = read_file(&inbox, OsStr::new(&name), MARKER_BYTES)?;
        if current_bytes != bytes || !same(&file.metadata()?, &current.metadata()?) {
            return Err(invalid("acknowledgement marker changed"));
        }
        let named_done = open(&inbox, ".done", OFlags::RDONLY | OFlags::DIRECTORY)?;
        if !same(&done.metadata()?, &named_done.metadata()?) {
            return Err(invalid("acknowledgement directory changed"));
        }
        self.validate_child("inbox", &inbox)?;
        validate_file(&inbox, OsStr::new(&name), &file)?;
        let _ = present(&done, OsStr::new(&name))?;
        rename(&inbox, &name, &done, &name)?;
        done.sync_all()?;
        inbox.sync_all()?;
        self.validate_child("inbox", &inbox)?;
        let named_done = open(&inbox, ".done", OFlags::RDONLY | OFlags::DIRECTORY)?;
        if !same(&done.metadata()?, &named_done.metadata()?) {
            return Err(invalid("acknowledgement directory changed"));
        }
        verify_handoff(&done, &name, &file, &bytes)
    }

    fn publish(
        &self,
        id: MailId,
        bytes: &[u8],
        sync: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        self.publish_with(id, bytes, sync, rename)
    }

    fn publish_with(
        &self,
        id: MailId,
        bytes: &[u8],
        sync: impl FnOnce(&File) -> io::Result<()>,
        rename: impl FnOnce(&File, &str, &File, &str) -> io::Result<()>,
    ) -> io::Result<()> {
        self.publish_named(&format!("{id}.json"), id, bytes, sync, rename)
    }

    fn queue_control(&self, control: &Control) -> io::Result<MailId> {
        let bytes = serde_json::to_vec(control)?;
        let outbox = self.child("outbox")?;
        if names(&outbox)?.len() >= CONTROL_OUTBOX_CAP {
            return Err(invalid("outbox is full; retry shortly"));
        }
        let id = fresh_id()?;
        self.publish_named(
            &format!("{id}{CONTROL_SUFFIX}"),
            id,
            &bytes,
            File::sync_all,
            rename,
        )?;
        Ok(id)
    }

    fn publish_named(
        &self,
        name: &str,
        id: MailId,
        bytes: &[u8],
        sync: impl FnOnce(&File) -> io::Result<()>,
        rename: impl FnOnce(&File, &str, &File, &str) -> io::Result<()>,
    ) -> io::Result<()> {
        let outbox = self.child("outbox")?;
        let temporary = format!(".{id}.tmp");
        let file = open(
            &outbox,
            temporary.as_str(),
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
        )?;
        let staged = Staged {
            directory: &outbox,
            name: &temporary,
            file,
        };
        (&staged.file).write_all(bytes)?;
        sync(&staged.file)?;
        outbox.sync_all()?;
        self.validate_child("outbox", &outbox)?;
        let (current, found) = read_file(&outbox, OsStr::new(&temporary), BODY_BYTES)?;
        if found != bytes || !same(&staged.file.metadata()?, &current.metadata()?) {
            return Err(invalid("outgoing staging bytes changed"));
        }
        let _ = present(&outbox, OsStr::new(name))?;
        rename(&outbox, &temporary, &outbox, name)?;
        outbox.sync_all()?;
        self.validate_child("outbox", &outbox)?;
        verify_handoff(&outbox, name, &staged.file, bytes)
    }
}

struct Staged<'a> {
    directory: &'a File,
    name: &'a str,
    file: File,
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        if validate_file(self.directory, OsStr::new(self.name), &self.file).is_ok() {
            let _ = rustix::fs::unlinkat(self.directory, self.name, AtFlags::empty());
            let _ = self.directory.sync_all();
        }
    }
}

fn fresh_id() -> io::Result<MailId> {
    let millis = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis(),
    )
    .map_err(io::Error::other)?;
    if millis > 0xffff_ffff_ffff {
        return Err(invalid("mail timestamp exceeds its identifier range"));
    }
    let mut entropy = [0; 16];
    getrandom::fill(&mut entropy).map_err(io::Error::other)?;
    Ok(MailId::from_parts(millis, u128::from_be_bytes(entropy)))
}

pub(super) fn run(
    command: &MailCommand,
    path: &Path,
    scoped_file_mail: bool,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mailbox = Mailbox::open(path)?;
    match command {
        MailCommand::Send(send) => {
            let bytes = mailbox.request(send)?;
            let id = fresh_id()?;
            mailbox.publish(id, &bytes, File::sync_all)?;
            writeln!(out, "mail queued {id}")
        }
        MailCommand::Read | MailCommand::List => {
            let items = mailbox.read()?;
            if items.is_empty() {
                return writeln!(out, "no unread mail");
            }
            let mut names_hint = matches!(command, MailCommand::Read) && !scoped_file_mail;
            for (item, body) in items {
                let from = match item.from {
                    MailFrom::Agent { agent } if std::mem::take(&mut names_hint) => {
                        format!("{agent}\nUse `dudes team` to see agent names.")
                    }
                    MailFrom::Agent { agent } => agent.to_string(),
                    MailFrom::Human => "human".to_owned(),
                };
                writeln!(
                    out,
                    "mail {}\nfrom {}\nact {}\nsubject {}",
                    item.id,
                    from,
                    match item.act {
                        MailAct::Message => "message",
                        MailAct::Request => "request",
                        MailAct::Card => "card",
                        MailAct::Ack => "ack",
                    },
                    Escaped(&item.subject)
                )?;
                if let Some(thread) = item.thread {
                    writeln!(
                        out,
                        "thread {thread} sequence {} members {}",
                        item.thread_seq,
                        item.members.len()
                    )?;
                }
                if matches!(command, MailCommand::Read) {
                    if let Some(body) = body {
                        writeln!(out, "{}", Escaped(&body))?;
                    }
                    if item.act == MailAct::Request {
                        if item.thread.is_some() {
                            writeln!(
                                out,
                                "reply: dudes mail send --reply-to {} --act message --body 'Your reply'",
                                item.id
                            )?;
                        } else {
                            writeln!(out, "reply unavailable: this mail has no thread")?;
                        }
                    }
                }
            }
            Ok(())
        }
        MailCommand::Done(id) if scoped_file_mail => {
            mailbox.queue_control(&Control::MailDone { marker: *id })?;
            writeln!(
                out,
                "mail done request queued for {id}; the daemon records it shortly"
            )
        }
        MailCommand::Done(id) => {
            mailbox.done(*id)?;
            writeln!(out, "mail done {id}")
        }
    }
}

pub(super) fn closing_ack(announcement: Lsn, path: &Path, out: &mut dyn Write) -> io::Result<()> {
    Mailbox::open(path)?.queue_control(&Control::ClosingAck { announcement })?;
    writeln!(
        out,
        "closing acknowledgement queued for announcement {}; the daemon records it shortly",
        announcement.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned_mailbox() -> (tempfile::TempDir, Mailbox) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(AgentId::from_parts(1, 1).to_string());
        std::fs::create_dir_all(path.join("outbox")).unwrap();
        std::fs::create_dir_all(path.join("inbox/.done")).unwrap();
        let mailbox = Mailbox::open(&path).unwrap();
        (root, mailbox)
    }

    fn inbox_marker(mailbox: &Mailbox, id: MailId) -> Vec<u8> {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "id": id,
            "from": {"kind": "human"},
            "to": {"kind": "agent", "agent": mailbox.id},
            "act": "message",
            "subject": "original",
            "body_len": 0,
        }))
        .unwrap();
        let inbox = mailbox.child("inbox").unwrap();
        let file = open(
            &inbox,
            format!("{id}.json").as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
        )
        .unwrap();
        (&file).write_all(&bytes).unwrap();
        bytes
    }

    fn replace_source(source: &File, name: &str) {
        rename(source, name, source, "retained-original").unwrap();
        let replacement =
            open(source, name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL).unwrap();
        (&replacement).write_all(b"substituted bytes").unwrap();
    }

    #[test]
    fn read_points_to_names_once_after_the_first_agent_sender() {
        let (root, mailbox) = owned_mailbox();
        let path = root.path().join(mailbox.id.to_string());
        for n in 1..=2 {
            let id = MailId::from_parts(1, n);
            let mut value: serde_json::Value =
                serde_json::from_slice(&inbox_marker(&mailbox, id)).unwrap();
            value["from"] = serde_json::json!({"kind":"agent","agent":AgentId::from_parts(1, 2)});
            std::fs::write(
                path.join("inbox").join(format!("{id}.json")),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        }
        let mut output = Vec::new();
        run(&MailCommand::Read, &path, false, &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.matches("Use `dudes team`").count(), 1);
        assert!(text.contains(&format!(
            "from {}\nUse `dudes team`",
            AgentId::from_parts(1, 2)
        )));
        let mut output = Vec::new();
        run(&MailCommand::List, &path, false, &mut output).unwrap();
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains("Use `dudes team`")
        );
    }

    #[test]
    fn publication_handoff_refuses_source_substitution() {
        let (_root, mailbox) = owned_mailbox();
        let id = MailId::from_parts(1, 3);
        let result = mailbox.publish_with(
            id,
            b"original request",
            File::sync_all,
            |source, from, destination, to| {
                replace_source(source, from);
                rename(source, from, destination, to)
            },
        );
        assert!(result.is_err(), "substituted request reported queued");
        let outbox = mailbox.child("outbox").unwrap();
        assert_eq!(
            read_file(&outbox, OsStr::new("retained-original"), BODY_BYTES)
                .unwrap()
                .1,
            b"original request"
        );
    }

    #[test]
    fn acknowledgement_handoff_refuses_source_substitution() {
        let (_root, mailbox) = owned_mailbox();
        let id = MailId::from_parts(1, 4);
        let bytes = inbox_marker(&mailbox, id);
        let result = mailbox.done_with(id, |source, from, destination, to| {
            replace_source(source, from);
            rename(source, from, destination, to)
        });
        assert!(result.is_err(), "substituted marker reported done");
        let inbox = mailbox.child("inbox").unwrap();
        assert_eq!(
            read_file(&inbox, OsStr::new("retained-original"), MARKER_BYTES)
                .unwrap()
                .1,
            bytes
        );
    }

    #[test]
    fn publication_handoff_allows_consumption_of_the_original_file() {
        let (_root, mailbox) = owned_mailbox();
        let id = MailId::from_parts(1, 5);
        mailbox
            .publish_with(
                id,
                b"original request",
                File::sync_all,
                |source, from, destination, to| {
                    rename(source, from, destination, to)?;
                    rustix::fs::unlinkat(destination, to, AtFlags::empty())?;
                    Ok(())
                },
            )
            .unwrap();
        let outbox = mailbox.child("outbox").unwrap();
        assert_eq!(names(&outbox).unwrap(), Vec::<OsString>::new());
    }

    #[test]
    fn acknowledgement_handoff_allows_consumption_of_the_original_file() {
        let (_root, mailbox) = owned_mailbox();
        let id = MailId::from_parts(1, 6);
        inbox_marker(&mailbox, id);
        mailbox
            .done_with(id, |source, from, destination, to| {
                rename(source, from, destination, to)?;
                rustix::fs::unlinkat(destination, to, AtFlags::empty())?;
                Ok(())
            })
            .unwrap();
        let inbox = mailbox.child("inbox").unwrap();
        assert_eq!(names(&inbox).unwrap(), vec![OsString::from(".done")]);
    }

    fn interfere(destination: &File, name: &str, expected: &[u8], change: &str) {
        match change {
            "replacement" | "symlink" => {
                rename(destination, name, destination, "retained-original").unwrap();
                if change == "symlink" {
                    rustix::fs::symlinkat("retained-original", destination, name).unwrap();
                } else {
                    let file = open(
                        destination,
                        name,
                        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                    )
                    .unwrap();
                    (&file).write_all(expected).unwrap();
                }
            }
            "hardlink" => {
                rustix::fs::linkat(
                    destination,
                    name,
                    destination,
                    "extra-link",
                    AtFlags::empty(),
                )
                .unwrap();
            }
            "relocation" => {
                rename(destination, name, destination, "retained-original").unwrap();
            }
            "changed bytes" | "changed consumed bytes" => {
                let file = open(destination, name, OFlags::WRONLY | OFlags::TRUNC).unwrap();
                (&file).write_all(b"changed").unwrap();
                if change == "changed consumed bytes" {
                    rustix::fs::unlinkat(destination, name, AtFlags::empty()).unwrap();
                }
            }
            _ => panic!("unknown handoff interference"),
        }
    }

    #[test]
    fn publication_handoff_refuses_destination_ambiguity_and_changed_bytes() {
        for change in [
            "replacement",
            "symlink",
            "hardlink",
            "relocation",
            "changed bytes",
            "changed consumed bytes",
        ] {
            let (_root, mailbox) = owned_mailbox();
            let id = MailId::from_parts(1, 7);
            let expected = b"original request";
            let result = mailbox.publish_with(
                id,
                expected,
                File::sync_all,
                |source, from, destination, to| {
                    rename(source, from, destination, to)?;
                    interfere(destination, to, expected, change);
                    Ok(())
                },
            );
            assert!(result.is_err(), "publication accepted {change}");
        }
    }

    #[test]
    fn acknowledgement_handoff_refuses_destination_ambiguity_and_changed_bytes() {
        for change in [
            "replacement",
            "symlink",
            "hardlink",
            "relocation",
            "changed bytes",
            "changed consumed bytes",
        ] {
            let (_root, mailbox) = owned_mailbox();
            let id = MailId::from_parts(1, 8);
            let expected = inbox_marker(&mailbox, id);
            let result = mailbox.done_with(id, |source, from, destination, to| {
                rename(source, from, destination, to)?;
                interfere(destination, to, &expected, change);
                Ok(())
            });
            assert!(result.is_err(), "acknowledgement accepted {change}");
        }
    }

    #[test]
    fn synced_publication_does_not_clobber_collision_or_leave_its_temporary_file() {
        let (_root, mailbox) = owned_mailbox();
        let outbox = mailbox.child("outbox").unwrap();
        let id = MailId::from_parts(1, 1);
        let original = b"original";
        mailbox.publish(id, original, File::sync_all).unwrap();
        let collision = mailbox.publish(id, b"different", File::sync_all);
        assert!(collision.is_err());
        assert_eq!(
            read_file(&outbox, OsStr::new(&format!("{id}.json")), BODY_BYTES)
                .unwrap()
                .1,
            original
        );
        assert_eq!(
            names(&outbox).unwrap(),
            vec![OsString::from(format!("{id}.json"))]
        );
    }

    #[test]
    fn failed_file_sync_keeps_the_final_name_absent_and_cleans_only_its_own_stage() {
        let (_root, mailbox) = owned_mailbox();
        let outbox = mailbox.child("outbox").unwrap();
        let id = MailId::from_parts(1, 2);
        let untouched = open(
            &outbox,
            "unrelated.tmp",
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
        )
        .unwrap();
        (&untouched).write_all(b"keep").unwrap();
        let failure = mailbox.publish(id, b"complete request", |_| {
            Err(io::Error::other("injected sync failure"))
        });
        assert!(failure.is_err());
        assert_eq!(
            names(&outbox).unwrap(),
            vec![OsString::from("unrelated.tmp")]
        );
        assert_eq!(
            read_file(&outbox, OsStr::new("unrelated.tmp"), BODY_BYTES)
                .unwrap()
                .1,
            b"keep"
        );
        mailbox
            .publish(id, b"complete request", File::sync_all)
            .unwrap();
        assert_eq!(
            read_file(&outbox, OsStr::new(&format!("{id}.json")), BODY_BYTES)
                .unwrap()
                .1,
            b"complete request"
        );
    }

    #[test]
    fn outgoing_bound_is_measured_on_the_complete_serialized_request() {
        let (_root, mailbox) = owned_mailbox();
        let request = MailSend {
            route: MailRoute::To(MailAddress::Everyone { everyone: true }),
            act: MailAct::Message,
            subject: Some("subject".to_owned()),
            body: Some("x".repeat(1_048_576)),
        };
        assert!(mailbox.request(&request).is_err());
        let request = MailSend {
            body: Some("x".repeat(4_097)),
            ..request
        };
        let encoded = mailbox.request(&request).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(value["to"], serde_json::json!({"everyone":true}));
        assert_eq!(value["body"], "x".repeat(4_097));
        assert!(value.get("from").is_none());
    }

    #[test]
    fn only_a_generated_reply_subject_can_shorten_to_fit_the_outgoing_bound() {
        let (root, mailbox) = owned_mailbox();
        let id = MailId::from_parts(1, 7);
        let mut marker: serde_json::Value =
            serde_json::from_slice(&inbox_marker(&mailbox, id)).unwrap();
        marker["subject"] = serde_json::json!("🦀🦀🦀");
        std::fs::write(
            root.path()
                .join(mailbox.id.to_string())
                .join("inbox")
                .join(format!("{id}.json")),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        let mut request = MailSend {
            route: MailRoute::Reply(id),
            act: MailAct::Message,
            subject: None,
            body: Some(String::new()),
        };
        let base = mailbox.request(&request).unwrap().len();
        let body = "x".repeat(usize::try_from(BODY_BYTES).unwrap() - base + 2);
        request.body = Some(body.clone());
        let bytes = mailbox.request(&request).unwrap();
        assert!(bytes.len() as u64 <= BODY_BYTES);
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["body"], body);
        assert!(value["subject"].as_str().unwrap().starts_with(REPLY_PREFIX));
        assert_eq!(value["subject"], "Re: 🦀🦀");
        request.subject = Some("Re: 🦀🦀🦀".to_owned());
        assert!(mailbox.request(&request).is_err());
    }
}
