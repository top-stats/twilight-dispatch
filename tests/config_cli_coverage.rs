//! The binary as the image runs it, `twilight-dispatch` and
//! `twilight-dispatch healthcheck`: its arguments, `.env` loading, log
//! format, exit codes and the runtimes it starts. Unlike tests/cli.rs, the
//! binary keeps `LLVM_PROFILE_FILE`, so the coverage job counts main.rs.
//!
//! No Redis, broker or Discord is needed: every run fails before it would
//! contact one, or is pointed at local sockets that never answer.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_twilight-dispatch");

/// How long one run of the binary may take. Only a failing test waits this
/// long.
const TIMEOUT: Duration = Duration::from_secs(60);

/// A working directory of the test's own without a `.env` (dotenvy looks for
/// one in the working directory and its parents).
fn work_dir(test: &str) -> PathBuf {
    let name = format!("twilight-dispatch-config-cli-{test}");
    let dir = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let _ = std::fs::remove_file(dir.join(".env"));
    dir
}

/// The binary with `args` (see `isolate`).
fn binary(args: &[&str], vars: &[(&str, String)], dir: &Path) -> Command {
    let mut command = Command::new(BIN);
    command.args(args);
    isolate(&mut command, vars, dir);
    command
}

/// `command` with only `vars` set (and `LLVM_PROFILE_FILE`, for coverage
/// runs), run in `dir` without input and with its output piped.
fn isolate(command: &mut Command, vars: &[(&str, String)], dir: &Path) {
    command
        .env_clear()
        .envs(vars.iter().map(|(name, value)| (*name, value)))
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
}

/// Runs `command` to the end (see `wait`).
fn run(mut command: Command) -> Output {
    wait(command.spawn().unwrap())
}

/// `child`'s output once it exits. A child still running after `TIMEOUT` is
/// killed and fails the test.
fn wait(mut child: Child) -> Output {
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + TIMEOUT;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().unwrap();
        }
        thread::sleep(Duration::from_millis(50));
    };
    let output = Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    };
    assert!(
        !timed_out,
        "still running after {TIMEOUT:?}: {}",
        output_text(&output)
    );
    output
}

/// Everything `pipe` yields, read on a thread of its own so that a chatty
/// child never blocks on a full pipe.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// stdout and stderr (tracing writes to stdout).
fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What a start needs, with the metrics server on a free port and the
/// Discord endpoints on a closed local port: Discord is never contacted.
fn service_vars() -> Vec<(&'static str, String)> {
    let closed = free_port();
    vec![
        ("BOT_TOKEN", "token".to_owned()),
        ("INTENTS", "513".to_owned()),
        ("SHARDS_TOTAL", "1".to_owned()),
        ("PROMETHEUS_HOST", "127.0.0.1".to_owned()),
        ("PROMETHEUS_PORT", free_port().to_string()),
        ("DISCORD_API_PROXY", format!("http://127.0.0.1:{closed}")),
        ("DISCORD_GATEWAY_URL", format!("ws://127.0.0.1:{closed}")),
    ]
}

/// The request on `stream`, read up to its blank line.
fn read_request(stream: &mut TcpStream) -> String {
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let mut request: Vec<u8> = Vec::new();
    let mut buffer = [0; 1024];
    while !request.ends_with(b"\r\n\r\n") {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

/// What follows the status line of every answer: an empty JSON object.
const ANSWER: &str = "\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";

/// A metrics server stand-in on `listener`: it reads each request, answers
/// it with `status` and sends it to the returned receiver.
fn answer(listener: TcpListener, status: &'static str) -> mpsc::Receiver<String> {
    let (requests, received) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                return;
            };
            // Read first: closing with the request unread would reset the
            // connection before the probe reads the answer.
            let request = read_request(&mut stream);
            let response = format!("HTTP/1.1 {status}{ANSWER}");
            let _ = stream.write_all(response.as_bytes());
            let _ = requests.send(request);
        }
    });
    received
}

/// `answer` on a free port of 127.0.0.1.
fn server(status: &'static str) -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (port, answer(listener, status))
}

/// Only `healthcheck` is a command. Anything else (a typo, `--version`) is
/// rejected with exit code 2 before the configuration is read, instead of
/// starting a second service against production Redis and the broker.
#[test]
fn unknown_arguments_exit_2_without_starting_the_service() {
    let dir = work_dir("unknown-argument");
    for argument in ["--version", "health", "--help"] {
        let output = run(binary(&[argument], &[], &dir));
        let text = output_text(&output);
        assert_eq!(output.status.code(), Some(2), "{argument}: {text}");
        assert!(
            text.contains(&format!("unknown argument {argument};")),
            "{text}"
        );
        assert!(
            text.contains("usage: twilight-dispatch [healthcheck]"),
            "{text}"
        );
        assert!(!text.contains("environment variable"), "{text}");
    }
}

/// A non-UTF-8 argument is rejected the same way; it used to panic.
#[cfg(unix)]
#[test]
fn a_non_utf8_argument_exits_2() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    let dir = work_dir("non-utf8-argument");
    let mut command = binary(&[], &[], &dir);
    command.arg(OsStr::from_bytes(b"health\xffcheck"));
    let output = run(command);
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(2), "{text}");
    assert!(
        text.contains("unknown argument health\u{fffd}check;"),
        "{text}"
    );
}

/// dotenvy applies the lines of `.env` before the first one it cannot parse
/// and none after it. That is reported, without the line (it may hold a
/// secret), and the service then fails on what is missing.
#[test]
fn a_malformed_env_file_is_reported_without_the_line() {
    let dir = work_dir("malformed-env");
    let env = "INTENTS=513\nBOT_TOKEN secretvalue\nREDIS_HOST=cache\n";
    std::fs::write(dir.join(".env"), env).unwrap();
    let vars = [
        ("BOT_TOKEN", "token".to_owned()),
        ("RABBIT_HOST", "mq".to_owned()),
    ];
    let output = run(binary(&[], &vars, &dir));
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(
        text.contains("Failed to load .env: cannot parse a line; it and the rest were skipped"),
        "{text}"
    );
    assert!(!text.contains("secretvalue"), "{text}");
    // INTENTS, before the bad line, was applied; REDIS_HOST, after it, was not.
    assert!(
        text.contains("missing environment variable REDIS_HOST"),
        "{text}"
    );
    assert!(!text.contains("variable INTENTS"), "{text}");
}

/// A `.env` that cannot be read (here: not UTF-8) is reported with the
/// reason, which does not quote the file.
#[test]
fn an_unreadable_env_file_is_reported() {
    let dir = work_dir("unreadable-env");
    let env = b"BOT_TOKEN=secret\xffvalue\n";
    std::fs::write(dir.join(".env"), env).unwrap();
    let output = run(binary(&[], &[], &dir));
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(text.contains("Failed to load .env: "), "{text}");
    assert!(!text.contains("cannot parse a line"), "{text}");
    assert!(!text.contains("secret"), "{text}");
    assert!(text.contains("missing environment variable"), "{text}");
}

/// No `.env` (the image has none) and a valid one are not reported.
#[test]
fn a_missing_or_valid_env_file_is_not_reported() {
    for (test, env) in [
        ("no-env", None),
        ("valid-env", Some("# settings\n\nINTENTS=513\n")),
    ] {
        let dir = work_dir(test);
        if let Some(env) = env {
            std::fs::write(dir.join(".env"), env).unwrap();
        }
        let output = run(binary(&[], &[], &dir));
        let text = output_text(&output);
        assert_eq!(output.status.code(), Some(1), "{test}: {text}");
        assert!(!text.contains("Failed to load .env"), "{test}: {text}");
        assert!(text.contains("missing environment variable"), "{text}");
        if env.is_some() {
            // The file's INTENTS was applied.
            assert!(!text.contains("variable INTENTS"), "{text}");
        }
    }
}

/// Container logs are no terminal: no colour codes in them.
#[test]
fn logs_have_no_colour_codes_when_stdout_is_not_a_terminal() {
    let dir = work_dir("no-colour");
    let runs: [&[&str]; 2] = [&[], &["--version"]];
    for args in runs {
        let output = run(binary(args, &[], &dir));
        let text = output_text(&output);
        assert!(text.contains("ERROR"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }
}

/// `healthcheck` (the image's HEALTHCHECK) exits 0 only when the metrics
/// server answers `GET /healthcheck` with 200. Without PROMETHEUS_HOST both
/// loopback addresses are probed.
#[test]
fn healthcheck_exits_with_the_health_of_the_metrics_server() {
    let (healthy, requests) = server("200 OK");
    let (unhealthy, _) = server("503 Service Unavailable");
    let closed = free_port();
    let dir = work_dir("healthcheck");
    for (port, host, code) in [
        (healthy, Some("127.0.0.1"), 0),
        (unhealthy, Some("127.0.0.1"), 1),
        (closed, Some("127.0.0.1"), 1),
        (closed, None, 1),
    ] {
        let mut vars = vec![("PROMETHEUS_PORT", port.to_string())];
        if let Some(host) = host {
            vars.push(("PROMETHEUS_HOST", host.to_owned()));
        }
        let output = run(binary(&["healthcheck"], &vars, &dir));
        assert_eq!(
            output.status.code(),
            Some(code),
            "port {port}, host {host:?}: {}",
            output_text(&output)
        );
    }
    let request = requests.recv_timeout(TIMEOUT).unwrap();
    assert!(
        request.starts_with("GET /healthcheck HTTP/1.1\r\n"),
        "{request:?}"
    );
}

/// The probe reads PROMETHEUS_PORT as the service does, so a value the
/// service refuses at startup, such as `+9000`, or a blank one, means the
/// default port 8005.
#[test]
fn healthcheck_reads_the_port_as_the_service_does() {
    let Ok(listener) = TcpListener::bind("127.0.0.1:8005") else {
        eprintln!("port 8005 is in use; skipped");
        return;
    };
    let requests = answer(listener, "200 OK");
    let dir = work_dir("healthcheck-port");
    for port in ["+9000", "09000", " ", "65536"] {
        let vars = [
            ("PROMETHEUS_HOST", "127.0.0.1".to_owned()),
            ("PROMETHEUS_PORT", port.to_owned()),
        ];
        let output = run(binary(&["healthcheck"], &vars, &dir));
        assert!(
            output.status.success(),
            "PROMETHEUS_PORT={port:?}: {}",
            output_text(&output)
        );
        requests.recv_timeout(TIMEOUT).unwrap();
    }
}

/// The probe runs on one thread, not with a worker thread per CPU: Docker
/// starts it every 30 s, for at most two connections.
#[cfg(target_os = "linux")]
#[test]
fn healthcheck_runs_on_a_single_thread() {
    use std::io::ErrorKind;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = work_dir("healthcheck-thread");
    let vars = [
        ("PROMETHEUS_HOST", "127.0.0.1".to_owned()),
        ("PROMETHEUS_PORT", port.to_string()),
    ];
    let mut child = binary(&["healthcheck"], &vars, &dir).spawn().unwrap();

    // Probing: connected, and waiting for an answer.
    let deadline = Instant::now() + TIMEOUT;
    let connection = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) => assert_eq!(err.kind(), ErrorKind::WouldBlock, "{err}"),
        }
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            panic!("did not probe: {}", output_text(&wait(child)));
        }
        thread::sleep(Duration::from_millis(50));
    };
    let status = std::fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
    // Closed without an answer: unhealthy.
    drop(connection);
    let output = wait(child);
    assert_eq!(output.status.code(), Some(1), "{}", output_text(&output));
    let threads = status.lines().find(|line| line.starts_with("Threads:"));
    assert_eq!(threads, Some("Threads:\t1"), "{status}");
}

/// A service that cannot reach Redis fails its startup with exit code 1,
/// after naming the obsolete variables that are set.
#[test]
fn startup_failures_exit_1_after_naming_obsolete_variables() {
    let dir = work_dir("startup-failure");
    let mut vars = service_vars();
    vars.extend([
        ("REDIS_URL", format!("redis://127.0.0.1:{}", free_port())),
        ("AMQP_URL", format!("amqp://127.0.0.1:{}/%2f", free_port())),
        ("CLUSTERS", "2".to_owned()),
        ("SHARDS_WAIT", "6".to_owned()),
    ]);
    let output = run(binary(&[], &vars, &dir));
    let text = output_text(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    for name in ["CLUSTERS", "SHARDS_WAIT"] {
        let warning = format!("{name} is no longer used and can be removed");
        assert!(text.contains(&warning), "{text}");
    }
}

/// `docker stop` while the service is still starting (here: waiting for a
/// Redis that never answers) stops it with exit code 0.
#[cfg(target_os = "linux")]
#[test]
fn sigterm_during_startup_exits_0() {
    use std::io::ErrorKind;

    // Bit `SIGTERM - 1` of a signal mask.
    const SIGTERM: u64 = 1 << (15 - 1);

    // Whether process `pid` has a handler for SIGTERM (`SigCgt`, the mask of
    // caught signals, in /proc).
    fn catches_sigterm(pid: u32) -> bool {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return false;
        };
        for line in status.lines() {
            if let Some(mask) = line.strip_prefix("SigCgt:") {
                let mask = u64::from_str_radix(mask.trim(), 16).unwrap();
                return (mask & SIGTERM) != 0;
            }
        }
        false
    }

    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    silent.set_nonblocking(true).unwrap();
    let port = silent.local_addr().unwrap().port();
    let dir = work_dir("sigterm-during-startup");
    let mut vars = service_vars();
    vars.extend([
        ("REDIS_URL", format!("redis://127.0.0.1:{port}")),
        ("AMQP_URL", format!("amqp://127.0.0.1:{port}/%2f")),
    ]);
    let mut child = binary(&[], &vars, &dir).spawn().unwrap();

    // Starting (connected to Redis) and listening for SIGTERM.
    let deadline = Instant::now() + TIMEOUT;
    let mut connection = None;
    while connection.is_none() || !catches_sigterm(child.id()) {
        if connection.is_none() {
            match silent.accept() {
                Ok((stream, _)) => connection = Some(stream),
                Err(err) => assert_eq!(err.kind(), ErrorKind::WouldBlock, "{err}"),
            }
        }
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            panic!("did not start: {}", output_text(&wait(child)));
        }
        thread::sleep(Duration::from_millis(50));
    }

    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(kill.success());
    let output = wait(child);
    let text = output_text(&output);
    assert!(output.status.success(), "{:?}: {text}", output.status);
    assert!(text.contains("Stopped during startup"), "{text}");
}

/// When the runtime cannot start (here: no file descriptor left for its I/O
/// driver), both commands say so and exit 1 before reading any settings.
#[cfg(target_os = "linux")]
#[test]
fn a_runtime_that_cannot_start_exits_1() {
    // Runs the binary ($0) with descriptors 0 to 3 only, of which stdin,
    // stdout and stderr take three (3 is closed in case it was inherited).
    // The runtime needs two (epoll and its waker); the one left serves the
    // dynamic loader at the start and the coverage profile at the exit.
    const SCRIPT: &str = r#"exec 3>&- && ulimit -n 4 && exec "$0" "$@""#;

    let dir = work_dir("no-runtime");
    let runs: [&[&str]; 2] = [&["healthcheck"], &[]];
    for args in runs {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", SCRIPT, BIN]).args(args);
        isolate(&mut command, &[], &dir);
        let output = run(command);
        let text = output_text(&output);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {text}");
        assert!(text.contains("Failed to start the runtime: "), "{text}");
        assert!(!text.contains("environment variable"), "{text}");
    }
}
