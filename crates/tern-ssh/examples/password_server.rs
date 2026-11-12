// Adapted from russh 0.64 examples/echoserver.rs (Apache-2.0) for the server handler shape.
//! A local SSH server that accepts one password, for checking tern's login flow by hand or
//! from a script. Never expose it: it binds 127.0.0.1 only.
//!
//! `cargo run -p tern-ssh --example password_server -- 2299 hunter2`
//!
//! With `--sftp DIR` it also serves the `sftp` subsystem over DIR (`/` is DIR, the login
//! directory is DIR/home), the same file service tern-ssh's tests use.
//!
//! With `--sudo` the shell prints `[sudo] password for test: ` after login, reads one line
//! without echo and checks it against the login password (`sudo: ok` or `Sorry, try again.`),
//! for checking tern's "Fill password" shortcut.
//!
//! With `--open` any client logs in with no credentials (the `none` method), so other SSH
//! apps can be measured connected without typing a password into them.
//!
//! With `--demo` the shell is a canned Ubuntu box (`web-01`, all data made up) with a MOTD, a
//! coloured prompt and a table of commands (`docker ps`, `htop`, `df -h`, `tail -f`, `sudo ...`),
//! for taking README screenshots of tern.
//!
//! With `--otp 123456` it behaves like a 2FA server: the password is only the first step
//! (partial success), then a keyboard-interactive "Verification code:" prompt with echo off
//! must be answered with the code.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stderr)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PrivateKey;
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};

#[path = "../tests/common/sftp.rs"]
mod sftp;

mod demo {
    //! The `--demo` shell: a line editor over a pty plus a table of canned commands. Every
    //! name, address and number is made up (10.0.0.0/8 and TEST-NET 203.0.113.0/24 only).

    const GREEN: &str = "\x1b[32m";
    const YELLOW: &str = "\x1b[33m";
    const RED: &str = "\x1b[31m";
    const CYAN: &str = "\x1b[36m";
    const DIM: &str = "\x1b[90m";
    const BOLD: &str = "\x1b[1m";
    const RESET: &str = "\x1b[0m";

    enum Mode {
        Prompt,
        /// Waiting for the sudo password; holds the command to run after it.
        SudoPassword(String),
        /// A `tail -f` is on screen; only Ctrl-C does anything.
        Following,
    }

    pub struct Shell {
        line: Vec<u8>,
        mode: Mode,
        /// 0 plain, 1 after ESC, 2 inside an `ESC [` sequence (arrow keys etc., swallowed).
        esc: u8,
    }

    impl Default for Shell {
        fn default() -> Self {
            Self {
                line: Vec::new(),
                mode: Mode::Prompt,
                esc: 0,
            }
        }
    }

    fn prompt(user: &str) -> String {
        format!("\x1b[01;32m{user}@web-01\x1b[00m:\x1b[01;34m~\x1b[00m$ ")
    }

    impl Shell {
        pub fn start(&mut self, user: &str) -> String {
            format!("{}\r\n{}", crlf(&motd()), prompt(user))
        }

        /// Feeds typed bytes; returns what to send back and whether to close the channel.
        pub fn feed(&mut self, user: &str, password: &str, data: &[u8]) -> (String, bool) {
            let mut out = String::new();
            for &b in data {
                if self.esc == 1 {
                    self.esc = if b == b'[' || b == b'O' { 2 } else { 0 };
                    continue;
                }
                if self.esc == 2 {
                    // A CSI sequence ends at its first byte in 0x40..=0x7e.
                    if (0x40..=0x7e).contains(&b) {
                        self.esc = 0;
                    }
                    continue;
                }
                if b == 0x1b {
                    self.esc = 1;
                    continue;
                }
                let silent = matches!(self.mode, Mode::SudoPassword(_));
                match (&self.mode, b) {
                    (Mode::Following, 0x03) => {
                        out.push_str(&format!("^C\r\n{}", prompt(user)));
                        self.mode = Mode::Prompt;
                    }
                    (Mode::Following, _) => {}
                    (_, 0x03) => {
                        self.line.clear();
                        self.mode = Mode::Prompt;
                        out.push_str(&format!("^C\r\n{}", prompt(user)));
                    }
                    (_, 0x04) if self.line.is_empty() => {
                        out.push_str("logout\r\n");
                        return (out, true);
                    }
                    (_, 0x7f | 0x08) => {
                        // Echo is off at the sudo prompt, so there is nothing to erase.
                        if self.line.pop().is_some() && !silent {
                            out.push_str("\x08 \x08");
                        }
                    }
                    (_, b'\r' | b'\n') => {
                        let line = String::from_utf8_lossy(&std::mem::take(&mut self.line))
                            .trim()
                            .to_owned();
                        out.push_str("\r\n");
                        if self.submit(user, password, &line, &mut out) {
                            return (out, true);
                        }
                    }
                    (_, b) if b >= 0x20 => {
                        self.line.push(b);
                        if !silent {
                            out.push(b as char);
                        }
                    }
                    _ => {}
                }
            }
            (out, false)
        }

        /// Handles one entered line in the current mode; true when the shell should close.
        fn submit(&mut self, user: &str, password: &str, line: &str, out: &mut String) -> bool {
            let line = match std::mem::replace(&mut self.mode, Mode::Prompt) {
                Mode::SudoPassword(cmd) => {
                    if line != password {
                        out.push_str(&format!(
                            "Sorry, try again.\r\n[sudo] password for {user}: "
                        ));
                        self.mode = Mode::SudoPassword(cmd);
                        return false;
                    }
                    cmd
                }
                _ => {
                    let words: Vec<&str> = line.split_whitespace().collect();
                    if words.first() == Some(&"sudo") {
                        out.push_str(&format!("[sudo] password for {user}: "));
                        self.mode = Mode::SudoPassword(words[1..].join(" "));
                        return false;
                    }
                    line.to_owned()
                }
            };
            match run(user, &line) {
                Reply::Text(t) => out.push_str(&crlf(&t)),
                Reply::Follow(t) => {
                    out.push_str(&crlf(&t));
                    self.mode = Mode::Following;
                    return false;
                }
                Reply::Exit => {
                    out.push_str("logout\r\n");
                    return true;
                }
            }
            out.push_str(&prompt(user));
            false
        }
    }

    enum Reply {
        Text(String),
        /// Output, then the command keeps running until Ctrl-C.
        Follow(String),
        Exit,
    }

    fn crlf(s: &str) -> String {
        s.replace("\r\n", "\n").replace('\n', "\r\n")
    }

    fn motd() -> String {
        "Welcome to Ubuntu 24.04.1 LTS (GNU/Linux 6.8.0-45-generic x86_64)

 * Documentation:  https://help.ubuntu.com
 * Management:     https://landscape.canonical.com
 * Support:        https://ubuntu.com/pro

  System information as of Tue Oct  6 09:12:41 UTC 2026

  System load:  0.42               Processes:             187
  Usage of /:   41.3% of 78.61GB   Users logged in:       1
  Memory usage: 38%                IPv4 address for eth0: 10.0.4.17
  Swap usage:   0%

0 updates can be applied immediately.

Last login: Mon Oct  5 18:03:22 2026 from 10.0.0.23
"
        .to_owned()
    }

    fn bar(pct: f32, width: usize) -> String {
        let filled = ((pct / 100.0) * width as f32).round() as usize;
        let color = if pct > 75.0 {
            RED
        } else if pct > 40.0 {
            YELLOW
        } else {
            GREEN
        };
        format!(
            "[{color}{}{DIM}{}{RESET} {pct:4.1}%]",
            "|".repeat(filled),
            " ".repeat(width - filled)
        )
    }

    /// A static one-screen htop-like snapshot.
    fn top() -> String {
        let mut s = String::new();
        for (i, pct) in [34.2f32, 12.8, 61.5, 8.1].iter().enumerate() {
            s.push_str(&format!("  {CYAN}{}{RESET} {}\n", i + 1, bar(*pct, 30)));
        }
        s.push_str(&format!(
            "  {CYAN}Mem{RESET} [{GREEN}||||||||||||{CYAN}||{YELLOW}|||||{DIM}             {RESET} 3.04G/7.75G]\n"
        ));
        s.push_str(&format!(
            "  {CYAN}Swp{RESET} [{DIM}                              {RESET}    0K/2.00G]\n\n"
        ));
        s.push_str(&format!(
            "  Tasks: {BOLD}64{RESET}, 211 thr; {GREEN}2{RESET} running   Load average: 0.42 0.38 0.35\n  Uptime: 41 days, 03:17:09\n\n"
        ));
        s.push_str(&format!(
            "\x1b[30;42m    PID USER       PRI  NI  VIRT   RES   SHR S CPU% MEM%   TIME+  Command      {RESET}\n"
        ));
        for row in [
            " 2214 root        20   0 1.2G  412M  38M S 31.5  5.2  3h12:44 api",
            " 2290 root        20   0  890M 301M  22M S 18.9  3.8  1h55:03 worker",
            " 1893 postgres    20   0  612M 188M 162M S  7.2  2.4 12:41.77 postgres",
            " 2051 root        20   0  145M  41M  11M S  3.1  0.5 27:08.15 redis-server",
            " 2108 caddy       20   0  762M  58M  31M S  1.4  0.7  4:20.31 caddy",
            "  912 root        20   0  1.8G  96M  48M S  0.7  1.2 19:07.02 dockerd",
            "    1 root        20   0  167M  13M 8.4M S  0.0  0.2  0:31.90 systemd",
        ] {
            s.push_str(row);
            s.push('\n');
        }
        s.push_str(&format!(
            "{DIM}F1{RESET}Help {DIM}F2{RESET}Setup {DIM}F3{RESET}Search {DIM}F6{RESET}SortBy {DIM}F10{RESET}Quit\n"
        ));
        s
    }

    fn logs() -> String {
        let lines = [
            ("09:12:01", "INFO", "api: listening on 0.0.0.0:8080"),
            (
                "09:12:02",
                "INFO",
                "worker: connected to redis 10.0.4.21:6379",
            ),
            ("09:12:04", "INFO", "api: GET /healthz 200 1ms"),
            ("09:12:09", "INFO", "api: POST /v1/orders 201 38ms"),
            (
                "09:12:11",
                "INFO",
                "worker: job send_receipt id=4821 done in 212ms",
            ),
            (
                "09:12:15",
                "WARN",
                "api: slow query (412ms) SELECT * FROM orders WHERE user_id = $1",
            ),
            ("09:12:18", "INFO", "api: GET /v1/orders/9917 200 9ms"),
            (
                "09:12:22",
                "INFO",
                "worker: job resize_image id=4822 done in 640ms",
            ),
            (
                "09:12:26",
                "ERROR",
                "worker: job send_receipt id=4823 failed: smtp 203.0.113.25:587 timeout",
            ),
            (
                "09:12:27",
                "INFO",
                "worker: job send_receipt id=4823 retry 1/5 in 30s",
            ),
            ("09:12:31", "INFO", "api: GET /healthz 200 1ms"),
            (
                "09:12:36",
                "WARN",
                "api: rate limit near for 10.0.2.54 (92/100)",
            ),
            ("09:12:40", "INFO", "api: POST /v1/orders 201 41ms"),
            (
                "09:12:44",
                "INFO",
                "worker: job resize_image id=4824 done in 598ms",
            ),
            ("09:12:49", "INFO", "api: GET /healthz 200 1ms"),
        ];
        let mut s = String::new();
        for (t, level, msg) in lines {
            let c = match level {
                "WARN" => YELLOW,
                "ERROR" => RED,
                _ => GREEN,
            };
            s.push_str(&format!(
                "{DIM}2026-10-06T{t}Z{RESET} {c}{level:<5}{RESET} {msg}\n"
            ));
        }
        s
    }

    const PS: &str = "CONTAINER ID   IMAGE                COMMAND                  CREATED       STATUS                PORTS                                      NAMES
3f9a1c7d2b10   acme/api:2.14.1      \"/app/api --serve\"      3 weeks ago   Up 3 weeks (healthy)  0.0.0.0:8080->8080/tcp                     api
8c2e55a90d3f   acme/worker:2.14.1   \"/app/worker\"           3 weeks ago   Up 3 weeks                                                       worker
b71d0e4a6c88   postgres:16.4        \"docker-entrypoint.s…\"  5 weeks ago   Up 5 weeks (healthy)  127.0.0.1:5432->5432/tcp                   postgres
5d0a9f31e7b2   redis:7.4-alpine     \"docker-entrypoint.s…\"  5 weeks ago   Up 5 weeks            6379/tcp                                   redis
e4c8137b9a05   caddy:2.8            \"caddy run --config…\"   5 weeks ago   Up 5 weeks            0.0.0.0:80->80/tcp, 0.0.0.0:443->443/tcp   caddy
";

    const STATS: &str = "CONTAINER ID   NAME       CPU %     MEM USAGE / LIMIT   MEM %     NET I/O           BLOCK I/O         PIDS
3f9a1c7d2b10   api        31.52%    412.3MiB / 1GiB     40.26%    8.2GB / 11.4GB    12.3MB / 4.1MB    24
8c2e55a90d3f   worker     18.87%    301.0MiB / 1GiB     29.39%    1.9GB / 902MB     8.0MB / 1.2MB     17
b71d0e4a6c88   postgres   7.21%     188.4MiB / 2GiB     9.20%     3.1GB / 6.7GB     2.4GB / 9.8GB     15
5d0a9f31e7b2   redis      3.05%     41.2MiB / 512MiB    8.05%     640MB / 701MB     0B / 12.1MB       5
e4c8137b9a05   caddy      1.38%     58.0MiB / 256MiB    22.66%    14.2GB / 14.0GB   31.5MB / 0B       11
";

    const DF: &str = "Filesystem      Size  Used Avail Use% Mounted on
tmpfs           795M  1.2M  794M   1% /run
/dev/vda1        79G   33G   46G  42% /
tmpfs           3.9G     0  3.9G   0% /dev/shm
tmpfs           5.0M     0  5.0M   0% /run/lock
/dev/vda15      105M  6.1M   99M   6% /boot/efi
tmpfs           795M  4.0K  795M   1% /run/user/1000
";

    const FREE: &str =
        "               total        used        free      shared  buff/cache   available
Mem:           7.8Gi       3.0Gi       1.9Gi        14Mi       2.9Gi       4.8Gi
Swap:          2.0Gi          0B       2.0Gi
";

    fn ls_la(u: &str) -> String {
        format!(
            "total 36
drwxr-x--- 5 {u} {u} 4096 Oct  5 18:03 .
drwxr-xr-x 4 root root 4096 Aug 26 11:40 ..
-rw------- 1 {u} {u} 2210 Oct  5 18:02 .bash_history
-rw-r--r-- 1 {u} {u}  220 Aug 26 11:40 .bash_logout
-rw-r--r-- 1 {u} {u} 3771 Aug 26 11:40 .bashrc
drwx------ 2 {u} {u} 4096 Aug 26 11:41 .ssh
drwxrwxr-x 6 {u} {u} 4096 Oct  1 14:22 app
drwxrwxr-x 2 {u} {u} 4096 Sep 28 02:00 backups
-rw-rw-r-- 1 {u} {u} 1184 Sep 14 09:31 docker-compose.yml
-rw-rw-r-- 1 {u} {u}  412 Sep 30 16:45 notes.txt
"
        )
    }

    const GIT_LOG: &str = "\x1b[33m9c41e07\x1b[m \x1b[1;36m(\x1b[1;32mHEAD -> main\x1b[1;36m)\x1b[m api: cap order page size at 100
\x1b[33m2ab7d93\x1b[m worker: retry receipts with exponential backoff
\x1b[33m71f08ce\x1b[m db: add index on orders(user_id, created_at)
\x1b[33me5530a1\x1b[m caddy: serve /healthz without auth
\x1b[33m0d6bb42\x1b[m deps: bump axum to 0.7.9
";

    fn run(user: &str, line: &str) -> Reply {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(&cmd) = words.first() else {
            return Reply::Text(String::new());
        };
        let args = &words[1..];
        let text = |s: &str| Reply::Text(s.to_owned());
        match (cmd, args) {
            ("exit" | "logout", _) => Reply::Exit,
            ("docker", ["ps", ..]) => text(PS),
            ("docker", ["stats", ..]) => text(STATS),
            ("docker", ["logs", ..]) => Reply::Follow(logs()),
            ("tail", a) if a.contains(&"-f") => Reply::Follow(logs()),
            ("uptime", _) => {
                text(" 09:14:02 up 41 days,  3:17,  1 user,  load average: 0.42, 0.38, 0.35\n")
            }
            ("free", _) => text(FREE),
            ("df", _) => text(DF),
            ("ls", a) if a.iter().any(|x| x.starts_with('-') && x.contains('l')) => {
                Reply::Text(ls_la(user))
            }
            ("ls", _) => text("app  backups  docker-compose.yml  notes.txt\n"),
            ("whoami", _) => Reply::Text(format!("{user}\n")),
            ("hostname", _) => text("web-01\n"),
            ("uname", _) => text(
                "Linux web-01 6.8.0-45-generic #45-Ubuntu SMP PREEMPT_DYNAMIC Fri Aug 30 12:02:04 UTC 2024 x86_64 x86_64 x86_64 GNU/Linux\n",
            ),
            ("htop" | "top", _) => Reply::Text(top()),
            ("git", ["log", ..]) => text(GIT_LOG),
            _ => Reply::Text(format!("bash: {cmd}: command not found\n")),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn type_line(sh: &mut Shell, line: &str) -> (String, bool) {
            sh.feed("alice", "hunter2", format!("{line}\r").as_bytes())
        }

        #[test]
        fn echoes_and_runs_a_command() {
            let mut sh = Shell::default();
            let (out, close) = type_line(&mut sh, "hostname");
            assert!(!close);
            assert!(out.starts_with("hostname\r\nweb-01\r\n"), "{out:?}");
            assert!(out.ends_with("$ "));
        }

        #[test]
        fn backspace_edits_the_line() {
            let mut sh = Shell::default();
            let (out, _) = sh.feed("alice", "hunter2", b"whoamx\x7fi\r");
            assert!(out.contains("\x08 \x08"));
            assert!(out.contains("\r\nalice\r\n"), "{out:?}");
        }

        #[test]
        fn sudo_checks_the_login_password_without_echo() {
            let mut sh = Shell::default();
            let (out, _) = type_line(&mut sh, "sudo docker ps");
            assert!(out.ends_with("[sudo] password for alice: "));
            let (bad, _) = type_line(&mut sh, "nope");
            assert!(bad.contains("Sorry, try again."));
            assert!(!bad.contains("nope"));
            let (good, _) = type_line(&mut sh, "hunter2");
            assert!(!good.contains("hunter2"));
            assert!(good.contains("postgres"), "{good:?}");
        }

        #[test]
        fn follow_stays_until_ctrl_c_and_exit_closes() {
            let mut sh = Shell::default();
            let (out, _) = type_line(&mut sh, "tail -f /var/log/app.log");
            assert!(out.contains("ERROR") && !out.ends_with("$ "));
            let (ignored, _) = sh.feed("alice", "hunter2", b"x");
            assert!(ignored.is_empty());
            let (back, _) = sh.feed("alice", "hunter2", b"\x03");
            assert!(back.ends_with("$ "));
            assert!(type_line(&mut sh, "exit").1);
            let (unknown, _) = type_line(&mut Shell::default(), "frobnicate");
            assert!(unknown.contains("bash: frobnicate: command not found"));
        }
    }
}

struct PasswordServer {
    password: Arc<String>,
    otp: Option<Arc<String>>,
    /// The password step passed on this connection; the code is asked only after it.
    password_ok: bool,
    /// Served as `/` for the sftp subsystem, when asked for.
    sftp_root: Option<PathBuf>,
    /// Print a sudo prompt after login and check the reply (`--sudo`).
    sudo: bool,
    /// What has been typed at the sudo prompt so far.
    typed: Vec<u8>,
    /// The canned shell (`--demo`).
    demo: bool,
    /// Let anyone in with no credentials (`--open`), for benchmarking other clients.
    open: bool,
    user: String,
    shell: demo::Shell,
    /// Session channels not yet claimed by a subsystem.
    sessions: HashMap<ChannelId, Channel<Msg>>,
    /// The channel the shell runs on. Bytes on any other (an SFTP channel) are not typing.
    shell_channel: Option<ChannelId>,
}

impl server::Handler for PasswordServer {
    type Error = russh::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        eprintln!("auth_none user={user} accepted={}", self.open);
        self.user = user.to_owned();
        Ok(if self.open {
            Auth::Accept
        } else {
            Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            }
        })
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        let ok = password == self.password.as_str();
        eprintln!("auth_password user={user} accepted={ok}");
        self.user = user.to_owned();
        Ok(if ok && self.otp.is_some() {
            self.password_ok = true;
            Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::KeyboardInteractive][..])),
                partial_success: true,
            }
        } else if ok {
            Auth::Accept
        } else {
            Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
                partial_success: false,
            }
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let (Some(code), true) = (self.otp.clone(), self.password_ok) else {
            return Ok(Auth::reject());
        };
        let Some(mut response) = response else {
            return Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: vec![("Verification code: ".into(), false)].into(),
            });
        };
        let given = response.next().unwrap_or_default();
        let ok = given.as_ref() == code.as_bytes();
        eprintln!("auth_keyboard_interactive user={user} accepted={ok}");
        Ok(if ok { Auth::Accept } else { Auth::reject() })
    }

    /// Carries `-L` / `-D` connections: dials the destination from here and bridges it, so a
    /// forward can be checked end to end against 127.0.0.1.
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _: &str,
        _: u32,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        let dest = (host.to_owned(), port as u16);
        match tokio::net::TcpStream::connect(dest).await {
            Ok(mut tcp) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
                });
            }
            Err(e) => eprintln!("direct-tcpip {host}:{port} failed: {e}"),
        }
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.sessions.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        ch: ChannelId,
        name: &str,
        s: &mut Session,
    ) -> Result<(), Self::Error> {
        match (name, self.sftp_root.clone(), self.sessions.remove(&ch)) {
            ("sftp", Some(root), Some(channel)) => {
                s.channel_success(ch)?;
                tokio::spawn(russh_sftp::server::run(
                    channel.into_stream(),
                    sftp::FsHandler::new(root),
                ));
            }
            _ => s.channel_failure(ch)?,
        }
        Ok(())
    }

    async fn shell_request(&mut self, ch: ChannelId, s: &mut Session) -> Result<(), Self::Error> {
        s.channel_success(ch)?;
        self.shell_channel = Some(ch);
        if self.demo {
            let out = self.shell.start(&self.user);
            s.data(ch, out)?;
            return Ok(());
        }
        s.data(ch, &b"password_server: logged in\r\n"[..])?;
        if self.sudo {
            s.data(ch, &b"[sudo] password for test: "[..])?;
        }
        Ok(())
    }

    async fn data(
        &mut self,
        ch: ChannelId,
        data: &[u8],
        s: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.shell_channel != Some(ch) {
            return Ok(());
        }
        if self.demo {
            let (out, close) = self.shell.feed(&self.user, &self.password, data);
            if !out.is_empty() {
                s.data(ch, out)?;
            }
            if close {
                s.eof(ch)?;
                s.close(ch)?;
            }
            return Ok(());
        }
        if !self.sudo {
            return Ok(());
        }
        for &b in data {
            if b != b'\r' && b != b'\n' {
                self.typed.push(b);
                continue;
            }
            let ok = self.typed == self.password.as_bytes();
            eprintln!("sudo reply accepted={ok} length={}", self.typed.len());
            self.typed.clear();
            let reply: &[u8] = if ok {
                b"\r\nsudo: ok\r\n"
            } else {
                b"\r\nSorry, try again.\r\n[sudo] password for test: "
            };
            s.data(ch, reply)?;
        }
        Ok(())
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let otp = args.iter().position(|a| a == "--otp").map(|i| {
        let code = args.get(i + 1).cloned().expect("--otp needs a code");
        args.drain(i..=i + 1);
        Arc::new(code)
    });
    let sudo = args
        .iter()
        .position(|a| a == "--sudo")
        .map(|i| args.remove(i))
        .is_some();
    let demo = args
        .iter()
        .position(|a| a == "--demo")
        .map(|i| args.remove(i))
        .is_some();
    let open = args
        .iter()
        .position(|a| a == "--open")
        .map(|i| args.remove(i))
        .is_some();
    let sftp_root = args.iter().position(|a| a == "--sftp").map(|i| {
        let dir = PathBuf::from(args.get(i + 1).cloned().expect("--sftp needs a directory"));
        args.drain(i..=i + 1);
        std::fs::create_dir_all(dir.join("home")).expect("create the sftp home");
        dir
    });
    let mut args = args.into_iter();
    let port: u16 = args.next().map_or(2299, |p| p.parse().expect("port"));
    let password = Arc::new(args.next().unwrap_or_else(|| "hunter2".into()));
    let config = Arc::new(server::Config {
        // Only the password is offered at first; the code is offered after it, as sshd's
        // `AuthenticationMethods password,keyboard-interactive` does.
        methods: if open {
            MethodSet::from(&[MethodKind::None, MethodKind::Password][..])
        } else {
            MethodSet::from(&[MethodKind::Password][..])
        },
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        // A fixed key, so a restarted server is the same host to known_hosts (reconnect checks).
        keys: vec![PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[7; 32]),
        )],
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    eprintln!("password_server listening on 127.0.0.1:{port}");
    while let Ok((stream, _)) = listener.accept().await {
        let handler = PasswordServer {
            password: password.clone(),
            otp: otp.clone(),
            password_ok: false,
            sftp_root: sftp_root.clone(),
            sudo,
            demo,
            open,
            user: String::new(),
            shell: demo::Shell::default(),
            typed: Vec::new(),
            sessions: HashMap::new(),
            shell_channel: None,
        };
        let config = config.clone();
        tokio::spawn(async move {
            if let Ok(running) = server::run_stream(config, stream, handler).await {
                let _ = running.await;
            }
        });
    }
}
