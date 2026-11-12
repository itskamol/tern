//! One tern per settings folder. Two processes writing `hosts.json` and the vault would each
//! overwrite what the other saved, so the first holds a lock on the folder and listens on a
//! socket beside it; a second launch (`tern web-01` from a shell) hands its host to the first,
//! which opens it and comes to the front, and exits.

use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::time::Duration;

use gpui::App;

const LOCK_FILE: &str = "tern.lock";
#[cfg(unix)]
const SOCKET_FILE: &str = "tern.sock";
/// A request is one line: the host to open, or nothing for "come to the front".
const MAX_REQUEST: u64 = 1024;
const ANSWER_WITHIN: Duration = Duration::from_secs(3);
const ACCEPTED: &[u8] = b"ok\n";

pub enum Claim {
    /// This process is the first: keep the instance for as long as it runs.
    First(Instance),
    /// The running tern took the request; this one has nothing left to do.
    Handed,
    /// Another tern holds the folder but did not take the request.
    Unanswered(io::Error),
}

/// The folder's lock, and the socket later launches knock on.
pub struct Instance {
    lock: Option<std::fs::File>,
    #[cfg(unix)]
    listener: Option<std::os::unix::net::UnixListener>,
}

/// Takes the settings folder `dir` for this process, or hands `target` to the tern holding it.
/// A folder that cannot be locked at all (read-only, unsupported) does not stop tern starting.
pub fn claim(dir: &Path, target: Option<&str>) -> Claim {
    let lock = std::fs::create_dir_all(dir).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK_FILE))
    });
    let lock = match lock {
        Ok(lock) => lock,
        Err(e) => {
            tracing::warn!(error = %e, "instance_lock_unavailable");
            return Claim::First(Instance::unlocked());
        }
    };
    match lock.try_lock() {
        Ok(()) => Claim::First(Instance::listen(dir, lock)),
        Err(std::fs::TryLockError::WouldBlock) => match hand_over(dir, target) {
            Ok(()) => Claim::Handed,
            Err(e) => Claim::Unanswered(e),
        },
        Err(std::fs::TryLockError::Error(e)) => {
            tracing::warn!(error = %e, "instance_lock_unavailable");
            Claim::First(Instance::unlocked())
        }
    }
}

impl Instance {
    fn unlocked() -> Self {
        Self {
            lock: None,
            #[cfg(unix)]
            listener: None,
        }
    }

    fn listen(dir: &Path, lock: std::fs::File) -> Self {
        #[cfg(unix)]
        {
            // Left behind by a tern that did not exit cleanly; the lock says none runs now.
            let path = dir.join(SOCKET_FILE);
            let _ = std::fs::remove_file(&path);
            let listener = std::os::unix::net::UnixListener::bind(&path)
                .and_then(|l| {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
                    Ok(l)
                })
                .inspect_err(|e| tracing::warn!(error = %e, "instance_socket_unavailable"))
                .ok();
            Self {
                lock: Some(lock),
                listener,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = dir;
            Self { lock: Some(lock) }
        }
    }

    /// Runs `on_request` on the UI thread for each later launch, with the host it asked for.
    /// The lock is held until the process exits.
    pub fn serve(self, cx: &mut App, on_request: impl Fn(Option<String>, &mut App) + 'static) {
        let Self {
            lock,
            #[cfg(unix)]
            listener,
        } = self;
        #[cfg(unix)]
        if let Some(listener) = listener {
            let (tx, rx) = async_channel::unbounded();
            let spawned = std::thread::Builder::new()
                .name("tern-instance".into())
                .spawn(move || {
                    let _held = lock;
                    for stream in listener.incoming().flatten() {
                        if let Some(target) = take_request(stream)
                            && tx.send_blocking(target).is_err()
                        {
                            break;
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "instance_listener_not_started");
            }
            cx.spawn(async move |cx| {
                while let Ok(target) = rx.recv().await {
                    cx.update(|cx| on_request(target, cx));
                }
            })
            .detach();
            return;
        }
        let _ = on_request;
        // Nobody to hand the file to: kept open, so locked, until the process exits.
        if let Some(lock) = lock {
            Box::leak(Box::new(lock));
        }
    }
}

/// Reads one request and answers it. `None` for one that is not a host tern would open.
#[cfg(unix)]
fn take_request(mut stream: std::os::unix::net::UnixStream) -> Option<Option<String>> {
    stream.set_read_timeout(Some(ANSWER_WITHIN)).ok()?;
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_REQUEST))
        .read_line(&mut line)
        .ok()?;
    let target = parse_request(&line)?;
    stream.write_all(ACCEPTED).ok()?;
    Some(target)
}

/// The host a request names: `None` inside for a bare "come to the front". Refused (`None`)
/// when it holds control characters or looks like a flag, as the command line refuses it.
fn parse_request(line: &str) -> Option<Option<String>> {
    let target = line.strip_suffix('\n')?.trim();
    if target.chars().any(char::is_control) || target.starts_with('-') {
        return None;
    }
    Some((!target.is_empty()).then(|| target.to_owned()))
}

#[cfg(unix)]
fn hand_over(dir: &Path, target: Option<&str>) -> io::Result<()> {
    let mut stream = std::os::unix::net::UnixStream::connect(dir.join(SOCKET_FILE))?;
    stream.set_read_timeout(Some(ANSWER_WITHIN))?;
    stream.set_write_timeout(Some(ANSWER_WITHIN))?;
    stream.write_all(format!("{}\n", target.unwrap_or_default()).as_bytes())?;
    let mut answer = Vec::new();
    stream
        .take(ACCEPTED.len() as u64)
        .read_to_end(&mut answer)?;
    if answer == ACCEPTED {
        Ok(())
    } else {
        Err(io::Error::other("the running tern refused the request"))
    }
}

#[cfg(not(unix))]
fn hand_over(_dir: &Path, _target: Option<&str>) -> io::Result<()> {
    Err(io::Error::other("tern is already running"))
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn first(dir: &Path) -> Instance {
        match claim(dir, None) {
            Claim::First(instance) => instance,
            _ => panic!("the first claim did not get the folder"),
        }
    }

    #[test]
    fn a_second_launch_hands_its_host_to_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let instance = first(dir.path());
        let listener = instance.listener.unwrap();
        let served = std::thread::spawn(move || take_request(listener.incoming().next()?.ok()?));
        assert!(matches!(claim(dir.path(), Some("web-01")), Claim::Handed));
        assert_eq!(served.join().unwrap(), Some(Some("web-01".into())));
    }

    #[test]
    fn a_holder_that_does_not_answer_is_reported_not_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut instance = first(dir.path());
        instance.listener = None;
        assert!(matches!(claim(dir.path(), None), Claim::Unanswered(_)));
        // Once it exits, the next launch is the first again. Other tests spawn processes in
        // parallel, and a child between fork and exec briefly shares the lock file's open
        // description (flock belongs to it, not to the fd), so the lock can outlive the drop
        // by a moment here; in the app the holder is a whole process that has exited.
        drop(instance);
        let freed = (0..100).any(|_| {
            let first = matches!(claim(dir.path(), None), Claim::First(_));
            if !first {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            first
        });
        assert!(
            freed,
            "the folder was not free within 2 s of the holder going"
        );
    }

    #[test]
    fn requests_name_a_host_or_nothing_and_never_a_flag() {
        assert_eq!(parse_request("web-01\n"), Some(Some("web-01".into())));
        assert_eq!(parse_request("\n"), Some(None));
        assert_eq!(parse_request("--help\n"), None);
        assert_eq!(parse_request("a\x1bb\n"), None);
        assert_eq!(parse_request("no newline"), None);
    }
}
