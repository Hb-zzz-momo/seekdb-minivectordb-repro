//! SIGTERM and SIGINT are a normal shutdown: exit status 0, the open
//! transaction rolled back, committed rows kept.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn start(dir: &PathBuf) -> (Child, BufReader<ChildStdout>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_minivectordb"))
        .arg("--data-dir")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    assert_eq!(line(&mut out), "{\"type\":\"ready\"}");
    (child, out)
}

fn line(out: &mut BufReader<ChildStdout>) -> String {
    let mut s = String::new();
    out.read_line(&mut s).unwrap();
    s.trim_end().to_string()
}

fn execute(child: &mut Child, out: &mut BufReader<ChildStdout>, id: u32, sql: &str) -> String {
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(stdin, "{{\"id\":{id},\"op\":\"execute\",\"sql\":\"{sql}\",\"search_mode\":\"exact\"}}").unwrap();
    stdin.flush().unwrap();
    line(out)
}

fn exits_cleanly_on(signal: i32) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("signal-{signal}"));
    let _ = std::fs::remove_dir_all(&dir);
    let (mut child, mut out) = start(&dir);
    assert_eq!(execute(&mut child, &mut out, 1, "CREATE TABLE t (id BIGINT PRIMARY KEY);"), "{\"id\":1,\"ok\":true}");
    assert_eq!(execute(&mut child, &mut out, 2, "INSERT INTO t VALUES (1);"), "{\"id\":2,\"ok\":true,\"affected_rows\":1}");
    assert_eq!(execute(&mut child, &mut out, 3, "BEGIN;"), "{\"id\":3,\"ok\":true}");
    assert_eq!(execute(&mut child, &mut out, 4, "INSERT INTO t VALUES (2);"), "{\"id\":4,\"ok\":true,\"affected_rows\":1}");
    // SAFETY: plain syscall wrapper; the pid is our own child.
    assert_eq!(unsafe { kill(child.id() as i32, signal) }, 0);
    assert_eq!(child.wait().unwrap().code(), Some(0), "signal {signal}");
    assert_eq!(line(&mut out), "", "nothing more on stdout");

    let (mut child, mut out) = start(&dir);
    assert_eq!(
        execute(&mut child, &mut out, 1, "SELECT id FROM t ORDER BY id;"),
        "{\"id\":1,\"ok\":true,\"columns\":[\"id\"],\"rows\":[[1]]}"
    );
    drop(child.stdin.take());
    assert_eq!(child.wait().unwrap().code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sigterm_is_a_normal_shutdown() {
    exits_cleanly_on(15);
}

#[test]
fn sigint_is_a_normal_shutdown() {
    exits_cleanly_on(2);
}
