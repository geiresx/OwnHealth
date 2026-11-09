//! Lane A test support: an in-process IMAP server with the commands the email poller uses
//! (CAPABILITY, STARTTLS, LOGIN, SELECT, CREATE, UID SEARCH/FETCH/STORE/MOVE/COPY, EXPUNGE, LOGOUT).
//! Plain TCP by default; a test hands it a TLS acceptor for implicit TLS or STARTTLS. std and tokio
//! only, so the API tests can include it with `#[path = ...]`.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Turns an accepted TCP stream into a TLS stream.
pub type Acceptor =
    Arc<dyn Fn(TcpStream) -> Pin<Box<dyn Future<Output = std::io::Result<Box<dyn Io>>> + Send>> + Send + Sync>;

#[derive(Clone)]
pub enum Mode {
    Plain,
    Implicit(Acceptor),
    StartTls(Acceptor),
}

#[derive(Debug, Clone)]
pub struct Msg {
    pub uid: u32,
    pub seen: bool,
    pub deleted: bool,
    pub raw: Vec<u8>,
}

#[derive(Debug)]
pub struct State {
    pub folders: BTreeMap<String, Vec<Msg>>,
    pub next_uid: u32,
    pub user: String,
    pub pass: String,
    pub logins: u32,
    pub failed_logins: u32,
    pub has_move: bool,
    pub has_uidplus: bool,
    /// Every command line received (LOGIN lines included), for the "no password in clear" checks.
    pub plain_lines: Vec<String>,
}

pub struct FakeImap {
    pub addr: SocketAddr,
    pub state: Arc<Mutex<State>>,
}

impl FakeImap {
    pub async fn start(user: &str, pass: &str, mode: Mode) -> std::io::Result<FakeImap> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(State {
            folders: BTreeMap::from([("INBOX".to_string(), Vec::new())]),
            next_uid: 1,
            user: user.into(),
            pass: pass.into(),
            logins: 0,
            failed_logins: 0,
            has_move: true,
            has_uidplus: true,
            plain_lines: Vec::new(),
        }));
        let st = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let st = Arc::clone(&st);
                let mode = mode.clone();
                tokio::spawn(async move {
                    let _ = serve(tcp, st, mode).await;
                });
            }
        });
        Ok(FakeImap { addr, state })
    }

    pub fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn deliver(&self, raw: Vec<u8>) -> u32 {
        let mut s = self.lock();
        let uid = s.next_uid;
        s.next_uid += 1;
        s.folders.entry("INBOX".into()).or_default().push(Msg {
            uid,
            seen: false,
            deleted: false,
            raw,
        });
        uid
    }

    pub fn count(&self, folder: &str) -> usize {
        self.lock().folders.get(folder).map_or(0, Vec::len)
    }

    pub fn all_seen(&self, folder: &str) -> bool {
        self.lock()
            .folders
            .get(folder)
            .is_some_and(|f| f.iter().all(|m| m.seen))
    }
}

async fn serve(tcp: TcpStream, st: Arc<Mutex<State>>, mode: Mode) -> std::io::Result<()> {
    match mode {
        Mode::Plain => session(Box::new(tcp), st, true).await,
        Mode::Implicit(acc) => session(acc(tcp).await?, st, true).await,
        Mode::StartTls(acc) => {
            let mut r = BufReader::new(tcp);
            r.get_mut()
                .write_all(b"* OK [CAPABILITY IMAP4rev1 STARTTLS LOGINDISABLED] fake ready\r\n")
                .await?;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).await? == 0 {
                    return Ok(());
                }
                let (tag, cmd) = line.trim_end().split_once(' ').unwrap_or(("*", ""));
                lock(&st).plain_lines.push(line.trim_end().to_string());
                if cmd.eq_ignore_ascii_case("STARTTLS") {
                    r.get_mut()
                        .write_all(format!("{tag} OK begin TLS\r\n").as_bytes())
                        .await?;
                    break;
                }
                r.get_mut()
                    .write_all(format!("{tag} NO STARTTLS first\r\n").as_bytes())
                    .await?;
            }
            let tls = acc(r.into_inner()).await?;
            session(tls, st, false).await
        }
    }
}

fn lock(st: &Mutex<State>) -> MutexGuard<'_, State> {
    st.lock().unwrap_or_else(|e| e.into_inner())
}

/// The arguments of a command: atoms and quoted strings.
fn args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut a = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => a.extend(chars.next()),
                    '"' => break,
                    _ => a.push(c),
                }
            }
            out.push(a);
        } else if c == '(' {
            let mut a = String::new();
            for c in chars.by_ref() {
                a.push(c);
                if c == ')' {
                    break;
                }
            }
            out.push(a);
        } else {
            let mut a = String::new();
            while let Some(&c) = chars.peek() {
                if c == ' ' {
                    break;
                }
                a.push(c);
                chars.next();
            }
            out.push(a);
        }
    }
    out
}

fn in_set(set: &str, uid: u32) -> bool {
    set.split(',').any(|p| match p.split_once(':') {
        Some((a, b)) => {
            let lo = a.parse().unwrap_or(u32::MAX);
            let hi = if b == "*" { u32::MAX } else { b.parse().unwrap_or(0) };
            (lo..=hi).contains(&uid)
        }
        None => p.parse() == Ok(uid),
    })
}

fn header_of(raw: &[u8]) -> Vec<u8> {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or_else(|| raw.to_vec(), |i| raw[..i + 4].to_vec())
}

async fn session(io: Box<dyn Io>, st: Arc<Mutex<State>>, greet: bool) -> std::io::Result<()> {
    let mut r = BufReader::new(io);
    if greet {
        r.get_mut().write_all(b"* OK fake IMAP ready\r\n").await?;
    }
    let mut selected: Option<String> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let line = line.trim_end().to_string();
        let (tag, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let a = args(rest);
        let cmd = a.first().map(|c| c.to_ascii_uppercase()).unwrap_or_default();
        let mut out: Vec<u8> = Vec::new();
        let ok = |out: &mut Vec<u8>, text: &str| out.extend_from_slice(format!("{tag} OK {text}\r\n").as_bytes());
        match cmd.as_str() {
            "CAPABILITY" => {
                let (mv, up) = {
                    let s = lock(&st);
                    (
                        if s.has_move { " MOVE" } else { "" },
                        if s.has_uidplus { " UIDPLUS" } else { "" },
                    )
                };
                out.extend_from_slice(format!("* CAPABILITY IMAP4rev1{mv}{up}\r\n").as_bytes());
                ok(&mut out, "CAPABILITY completed");
            }
            "NOOP" => ok(&mut out, "NOOP completed"),
            "LOGIN" => {
                let mut s = lock(&st);
                if a.get(1) == Some(&s.user) && a.get(2) == Some(&s.pass) {
                    s.logins += 1;
                    ok(&mut out, "LOGIN completed");
                } else {
                    s.failed_logins += 1;
                    out.extend_from_slice(
                        format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n").as_bytes(),
                    );
                }
            }
            "SELECT" => {
                let name = a.get(1).cloned().unwrap_or_default();
                let s = lock(&st);
                match s.folders.get(&name) {
                    Some(f) => {
                        out.extend_from_slice(
                            format!(
                                "* {} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY 7] UIDs valid\r\n* OK [UIDNEXT {}] next\r\n\
                                 * FLAGS (\\Seen \\Deleted)\r\n",
                                f.len(),
                                s.next_uid
                            )
                            .as_bytes(),
                        );
                        selected = Some(name);
                        out.extend_from_slice(format!("{tag} OK [READ-WRITE] SELECT completed\r\n").as_bytes());
                    }
                    None => out.extend_from_slice(format!("{tag} NO no such folder\r\n").as_bytes()),
                }
            }
            "CREATE" => {
                let name = a.get(1).cloned().unwrap_or_default();
                let mut s = lock(&st);
                if let std::collections::btree_map::Entry::Vacant(e) = s.folders.entry(name) {
                    e.insert(Vec::new());
                    ok(&mut out, "CREATE completed");
                } else {
                    out.extend_from_slice(format!("{tag} NO [ALREADYEXISTS] exists\r\n").as_bytes());
                }
            }
            "EXPUNGE" => {
                let mut s = lock(&st);
                if let Some(f) = selected.as_ref().and_then(|n| s.folders.get_mut(n)) {
                    for i in (0..f.len()).rev() {
                        if f[i].deleted {
                            f.remove(i);
                            out.extend_from_slice(format!("* {} EXPUNGE\r\n", i + 1).as_bytes());
                        }
                    }
                }
                ok(&mut out, "EXPUNGE completed");
            }
            "LOGOUT" => {
                out.extend_from_slice(format!("* BYE bye\r\n{tag} OK LOGOUT completed\r\n").as_bytes());
                r.get_mut().write_all(&out).await?;
                return Ok(());
            }
            "UID" => uid_command(&st, selected.as_deref(), tag, &a[1..], &mut out),
            _ => out.extend_from_slice(format!("{tag} BAD unknown command\r\n").as_bytes()),
        }
        r.get_mut().write_all(&out).await?;
        r.get_mut().flush().await?;
    }
}

fn uid_command(st: &Mutex<State>, selected: Option<&str>, tag: &str, a: &[String], out: &mut Vec<u8>) {
    let mut s = lock(st);
    let Some(folder) = selected.map(str::to_string) else {
        out.extend_from_slice(format!("{tag} NO select first\r\n").as_bytes());
        return;
    };
    let sub = a.first().map(|c| c.to_ascii_uppercase()).unwrap_or_default();
    let set = a.get(1).cloned().unwrap_or_default();
    match sub.as_str() {
        "SEARCH" => {
            let f = s.folders.get(&folder).cloned().unwrap_or_default();
            let undeleted = a[1..].iter().any(|w| w.eq_ignore_ascii_case("UNDELETED"));
            let uids: Vec<String> = f
                .iter()
                .filter(|m| !(undeleted && m.deleted))
                .map(|m| m.uid.to_string())
                .collect();
            let sep = if uids.is_empty() { "" } else { " " };
            out.extend_from_slice(format!("* SEARCH{sep}{}\r\n", uids.join(" ")).as_bytes());
        }
        "FETCH" => {
            let query = a[2..].join(" ").to_ascii_uppercase();
            let f = s.folders.get(&folder).cloned().unwrap_or_default();
            for (i, m) in f.iter().enumerate().filter(|(_, m)| in_set(&set, m.uid)) {
                out.extend_from_slice(format!("* {} FETCH (UID {}", i + 1, m.uid).as_bytes());
                if query.contains("RFC822.SIZE") {
                    out.extend_from_slice(format!(" RFC822.SIZE {}", m.raw.len()).as_bytes());
                }
                let section = if query.contains("BODY.PEEK[HEADER]") {
                    Some(("BODY[HEADER]", header_of(&m.raw)))
                } else if query.contains("BODY.PEEK[]") {
                    Some(("BODY[]", m.raw.clone()))
                } else {
                    None
                };
                if let Some((name, bytes)) = section {
                    out.extend_from_slice(format!(" {name} {{{}}}\r\n", bytes.len()).as_bytes());
                    out.extend_from_slice(&bytes);
                }
                out.extend_from_slice(b")\r\n");
            }
        }
        "STORE" => {
            let what = a[2..].join(" ").to_ascii_uppercase();
            if let Some(f) = s.folders.get_mut(&folder) {
                for (i, m) in f.iter_mut().enumerate().filter(|(_, m)| in_set(&set, m.uid)) {
                    m.seen |= what.contains("\\SEEN");
                    m.deleted |= what.contains("\\DELETED");
                    let flags = [(m.seen, "\\Seen"), (m.deleted, "\\Deleted")]
                        .iter()
                        .filter(|(on, _)| *on)
                        .map(|(_, f)| *f)
                        .collect::<Vec<_>>()
                        .join(" ");
                    out.extend_from_slice(format!("* {} FETCH (UID {} FLAGS ({flags}))\r\n", i + 1, m.uid).as_bytes());
                }
            }
        }
        "MOVE" | "COPY" => {
            let dest = a.get(2).cloned().unwrap_or_default();
            if !s.folders.contains_key(&dest) {
                out.extend_from_slice(format!("{tag} NO [TRYCREATE] no such folder\r\n").as_bytes());
                return;
            }
            let picked: Vec<Msg> = s.folders[&folder]
                .iter()
                .filter(|m| in_set(&set, m.uid))
                .cloned()
                .collect();
            for m in picked {
                let uid = s.next_uid;
                s.next_uid += 1;
                if let Some(d) = s.folders.get_mut(&dest) {
                    d.push(Msg { uid, ..m });
                }
            }
            if sub == "MOVE"
                && let Some(f) = s.folders.get_mut(&folder)
            {
                for i in (0..f.len()).rev() {
                    if in_set(&set, f[i].uid) {
                        f.remove(i);
                        out.extend_from_slice(format!("* {} EXPUNGE\r\n", i + 1).as_bytes());
                    }
                }
            }
        }
        "EXPUNGE" if s.has_uidplus => {
            if let Some(f) = s.folders.get_mut(&folder) {
                for i in (0..f.len()).rev() {
                    if f[i].deleted && in_set(&set, f[i].uid) {
                        f.remove(i);
                        out.extend_from_slice(format!("* {} EXPUNGE\r\n", i + 1).as_bytes());
                    }
                }
            }
        }
        _ => {
            out.extend_from_slice(format!("{tag} BAD unknown UID command\r\n").as_bytes());
            return;
        }
    }
    out.extend_from_slice(format!("{tag} OK UID {sub} completed\r\n").as_bytes());
}
