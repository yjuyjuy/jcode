#![cfg_attr(test, allow(clippy::await_holding_lock))]

use super::socket::sibling_socket_path;
#[cfg(unix)]
use super::socket::{
    daemon_lock_path, detach_daemon_stdio, server_start_matches_existing_server,
    try_acquire_daemon_lock,
};
use super::{
    ReloadPhase, ReloadState, ReloadWaitStatus, await_reload_handoff, cleanup_socket_pair,
    clear_reload_marker, inspect_reload_wait_status, publish_reload_socket_ready,
    reload_marker_active, reload_marker_path, reload_process_alive, write_reload_state,
};
#[cfg(unix)]
use super::{connect_socket, reap_stale_socket_if_dead};
#[cfg(unix)]
use crate::transport::Listener;
use std::time::Duration;

#[test]
fn sibling_socket_path_roundtrip() {
    let main = std::path::PathBuf::from("/tmp/jcode.sock");
    let debug = std::path::PathBuf::from("/tmp/jcode-debug.sock");

    assert_eq!(sibling_socket_path(&main), Some(debug.clone()));
    assert_eq!(sibling_socket_path(&debug), Some(main));
}

#[test]
fn cleanup_socket_pair_removes_main_and_debug_files() {
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let dir = std::env::temp_dir();
    let main = dir.join(format!("jcode-test-{}.sock", stamp));
    let debug = dir.join(format!("jcode-test-{}-debug.sock", stamp));

    std::fs::write(&main, b"").expect("create main socket placeholder");
    std::fs::write(&debug, b"").expect("create debug socket placeholder");

    cleanup_socket_pair(&main);

    assert!(!main.exists(), "main socket file should be removed");
    assert!(!debug.exists(), "debug socket file should be removed");
}

#[cfg(unix)]
#[tokio::test]
async fn connect_socket_preserves_refused_socket_path() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("jcode.sock");

    {
        let _listener = Listener::bind(&socket_path).expect("bind listener");
    }

    assert!(
        socket_path.exists(),
        "listener drop should leave the socket path behind for stale-socket checks"
    );

    let err = connect_socket(&socket_path)
        .await
        .expect_err("connect should fail once the listener is gone");
    assert!(
        err.to_string().contains("refused the connection"),
        "unexpected error: {err:#}"
    );
    assert!(
        socket_path.exists(),
        "connect_socket should not unlink the socket path on connection refusal"
    );
}

#[cfg(unix)]
#[test]
fn daemon_lock_serializes_server_processes() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    let lock_path = daemon_lock_path();
    let first = try_acquire_daemon_lock(&lock_path)
        .expect("acquire first daemon lock")
        .expect("first daemon lock should succeed");
    let second = try_acquire_daemon_lock(&lock_path).expect("acquire second daemon lock");
    assert!(second.is_none(), "second daemon lock should fail");
    drop(first);

    let third = try_acquire_daemon_lock(&lock_path)
        .expect("acquire third daemon lock")
        .expect("third daemon lock should succeed after release");
    drop(third);

    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn reap_stale_socket_removes_dead_socket_pair_and_lock() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    let socket = temp.path().join("jcode.sock");
    let debug = temp.path().join("jcode-debug.sock");
    let lock = daemon_lock_path();

    // Simulate the post-upgrade/crash state: socket + debug + lock files left
    // behind, but no process is listening on the socket.
    std::fs::write(&socket, b"").expect("write stale socket");
    std::fs::write(&debug, b"").expect("write stale debug socket");
    std::fs::write(&lock, b"").expect("write stale lock");

    let reaped = reap_stale_socket_if_dead(&socket).await;
    assert!(reaped, "a dead socket with no listener should be reaped");
    assert!(!socket.exists(), "stale socket should be removed");
    assert!(!debug.exists(), "stale debug socket should be removed");
    assert!(!lock.exists(), "stale daemon lock should be removed");

    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn reap_stale_socket_spares_live_listener() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    let socket = temp.path().join("jcode.sock");
    // A live listener means a daemon is bound; reaping must be a no-op.
    let listener = Listener::bind(&socket).expect("bind listener");

    let reaped = reap_stale_socket_if_dead(&socket).await;
    assert!(!reaped, "a live listener must never be reaped");
    assert!(socket.exists(), "live socket must be left intact");

    drop(listener);
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn reap_stale_socket_spares_socket_when_lock_is_held() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    let socket = temp.path().join("jcode.sock");
    std::fs::write(&socket, b"").expect("write stale-looking socket");

    // Hold the daemon lock, emulating a live daemon whose socket probe happens
    // to be momentarily unanswerable. The reaper must not unlink the socket.
    let lock_path = daemon_lock_path();
    let held = try_acquire_daemon_lock(&lock_path)
        .expect("acquire lock")
        .expect("lock should be free");

    let reaped = reap_stale_socket_if_dead(&socket).await;
    assert!(
        !reaped,
        "socket must be spared while the daemon lock is held"
    );
    assert!(
        socket.exists(),
        "socket must be left intact while lock is held"
    );

    drop(held);
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[cfg(unix)]
#[test]
fn existing_server_start_errors_are_detected() {
    assert!(server_start_matches_existing_server(
        "Error: Another jcode server process is already running for runtime dir /run/user/1000"
    ));
    assert!(server_start_matches_existing_server(
        "Error: Refusing to replace active server socket at /run/user/1000/jcode.sock"
    ));
    assert!(!server_start_matches_existing_server(
        "Error: failed to bind socket: permission denied"
    ));
}

#[test]
fn reload_marker_active_expires_stale_marker() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    let marker = reload_marker_path();
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_reload_state("test-request", "test-hash", ReloadPhase::Starting, None);
    assert!(reload_marker_active(Duration::from_secs(30)));
    std::thread::sleep(Duration::from_millis(5));
    assert!(!reload_marker_active(Duration::ZERO));
    assert!(!marker.exists(), "stale reload marker should be cleaned up");

    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[test]
fn reload_marker_active_for_recent_socket_ready_marker() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    write_reload_state("test-request", "test-hash", ReloadPhase::SocketReady, None);
    assert!(reload_marker_active(Duration::from_secs(30)));

    clear_reload_marker();
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[test]
fn publish_reload_socket_ready_updates_current_process_marker() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    write_reload_state(
        "test-request",
        "test-hash",
        ReloadPhase::Starting,
        Some("detail".to_string()),
    );
    publish_reload_socket_ready();

    let state = ReloadState::load().expect("reload state should exist");
    assert_eq!(state.phase, ReloadPhase::SocketReady);
    assert_eq!(state.request_id, "test-request");
    assert_eq!(state.hash, "test-hash");
    assert_eq!(state.detail.as_deref(), Some("detail"));

    clear_reload_marker();
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[test]
fn publish_reload_socket_ready_clears_marker_for_foreign_pid() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    ReloadState {
        request_id: "test-request".to_string(),
        hash: "test-hash".to_string(),
        phase: ReloadPhase::Starting,
        pid: std::process::id().saturating_add(1_000_000),
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail: None,
    }
    .write();

    publish_reload_socket_ready();
    assert!(
        ReloadState::load().is_none(),
        "foreign reload marker should be cleared"
    );

    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[tokio::test]
async fn inspect_reload_wait_status_reports_ready_for_socket_ready_marker() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    write_reload_state("test-request", "test-hash", ReloadPhase::SocketReady, None);

    let socket_path = temp.path().join("missing.sock");
    let status = inspect_reload_wait_status(&socket_path, Duration::from_secs(30), None).await;
    assert_eq!(status, ReloadWaitStatus::Ready);

    clear_reload_marker();
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn inspect_reload_wait_status_keeps_waiting_while_starting_marker_is_active_even_if_socket_is_live()
 {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    ReloadState {
        request_id: "test-request".to_string(),
        hash: "test-hash".to_string(),
        phase: ReloadPhase::Starting,
        pid: std::process::id(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail: None,
    }
    .write();

    let socket_path = temp.path().join("jcode.sock");
    let _listener = Listener::bind(&socket_path).expect("bind listener");

    let status = inspect_reload_wait_status(&socket_path, Duration::from_secs(30), None).await;
    assert_eq!(
        status,
        ReloadWaitStatus::Waiting {
            pid: Some(std::process::id())
        }
    );

    clear_reload_marker();
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[tokio::test]
async fn wait_for_reload_handoff_event_returns_promptly_when_no_event_arrives() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    let socket_path = temp.path().join("missing.sock");
    let started = std::time::Instant::now();
    crate::server::wait_for_reload_handoff_event(Some(std::process::id()), &socket_path).await;
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "reload handoff event wait should be a bounded edge wait, not an indefinite block"
    );

    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[tokio::test]
async fn inspect_reload_wait_status_reports_idle_without_marker_or_listener() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("missing.sock");

    let status = inspect_reload_wait_status(&socket_path, Duration::from_secs(30), None).await;
    assert_eq!(status, ReloadWaitStatus::Idle);
}

#[tokio::test]
async fn inspect_reload_wait_status_uses_last_known_pid_when_marker_missing() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("missing.sock");

    let status = inspect_reload_wait_status(
        &socket_path,
        Duration::from_secs(30),
        Some(std::process::id()),
    )
    .await;
    assert_eq!(
        status,
        ReloadWaitStatus::Waiting {
            pid: Some(std::process::id())
        }
    );
}

#[tokio::test]
async fn inspect_reload_wait_status_reports_failed_when_reload_pid_is_dead() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());
    let dead_pid = std::process::id().saturating_add(1_000_000);
    assert!(
        !reload_process_alive(dead_pid),
        "test requires a definitely-dead pid"
    );

    ReloadState {
        request_id: "test-request".to_string(),
        hash: "test-hash".to_string(),
        phase: ReloadPhase::Starting,
        pid: dead_pid,
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail: None,
    }
    .write();

    let socket_path = temp.path().join("missing.sock");
    let status = inspect_reload_wait_status(&socket_path, Duration::from_secs(30), None).await;
    assert!(matches!(status, ReloadWaitStatus::Failed(Some(_))));

    clear_reload_marker();
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[tokio::test]
async fn await_reload_handoff_returns_ready_after_marker_transition() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    ReloadState {
        request_id: "test-request".to_string(),
        hash: "test-hash".to_string(),
        phase: ReloadPhase::Starting,
        pid: std::process::id(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail: None,
    }
    .write();

    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_reload_state("test-request", "test-hash", ReloadPhase::SocketReady, None);
    });

    let socket_path = temp.path().join("missing.sock");
    let status = tokio::time::timeout(
        Duration::from_secs(2),
        await_reload_handoff(&socket_path, Duration::from_secs(30)),
    )
    .await
    .expect("await reload handoff should finish");
    assert_eq!(status, ReloadWaitStatus::Ready);

    clear_reload_marker();
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

#[tokio::test]
async fn await_reload_handoff_returns_failed_after_marker_transition() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());

    ReloadState {
        request_id: "test-request".to_string(),
        hash: "test-hash".to_string(),
        phase: ReloadPhase::Starting,
        pid: std::process::id(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        detail: None,
    }
    .write();

    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_reload_state(
            "test-request",
            "test-hash",
            ReloadPhase::Failed,
            Some("boom".to_string()),
        );
    });

    let socket_path = temp.path().join("missing.sock");
    let status = tokio::time::timeout(
        Duration::from_secs(2),
        await_reload_handoff(&socket_path, Duration::from_secs(30)),
    )
    .await
    .expect("await reload handoff should finish");
    assert_eq!(status, ReloadWaitStatus::Failed(Some("boom".to_string())));

    clear_reload_marker();
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
}

/// Regression: `jcode server start` daemon must survive its launching shell
/// exiting.
///
/// Root cause: the spawned daemon inherits `stderr` as a pipe whose only reader
/// is a drain thread inside the spawning client (see `spawn_server_notify`).
/// Once that client exits, the pipe has no readers and the next `eprintln!`
/// anywhere in the daemon writes to a broken pipe. Because SIGPIPE is ignored,
/// the Rust runtime turns that failed write into a panic ("failed printing to
/// stderr"), which tears the daemon down with no graceful shutdown log -- the
/// observed symptom. `detach_daemon_stdio` (called from `signal_ready_fd` once
/// the daemon is up and the client is free to leave) points stdout/stderr at
/// /dev/null so that write can never fail.
///
/// A full `server start` integration test would need a real installed binary
/// and a background daemon lifecycle, which is impractical inside a unit suite.
/// This instead reproduces the daemon's exact post-exit fd state -- fd 2 is a
/// pipe whose reader is gone -- inside a forked child (so the shared test
/// process's own stdio is never rewired), and asserts the mechanism at the
/// syscall level with deterministic raw writes: a write to that broken pipe
/// fails with EPIPE (the error the Rust runtime escalates to a fatal print
/// panic), and after `detach_daemon_stdio` the same write succeeds because fd 2
/// now points at /dev/null. The child reports both observations through a single
/// exit code, so there is no reliance on panic-after-fork behavior.
///
/// Coverage boundary: it exercises the stdio detach itself and its effect on
/// fd 1/2, not the surrounding `spawn_server_notify`/registry plumbing.
#[cfg(unix)]
#[test]
fn daemon_survives_parent_exit_after_stdio_detach() {
    // Exit codes the child uses to report what it observed.
    const OK: i32 = 0; // EPIPE before, write ok after (the fix works)
    const NO_EPIPE_BEFORE: i32 = 10; // broken pipe did not EPIPE (bad setup)
    const STILL_FAILS_AFTER: i32 = 11; // detach did not repair the write

    fn raw_write(fd: i32, bytes: &[u8]) -> std::io::Result<usize> {
        let n = unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe()");
    let (pipe_read, pipe_write) = (fds[0], fds[1]);

    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork()");

    if pid == 0 {
        // Child = the "daemon". SIGPIPE must be ignored so a broken-pipe write
        // returns EPIPE instead of killing us by signal -- this mirrors the
        // Rust runtime default under which the regression surfaces as a print
        // panic rather than a raw signal.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
            // Point stderr at the pipe's write end, then drop both original
            // pipe fds. With the read end closed, fd 2 is now a pipe with no
            // readers: exactly the daemon's state once its launching client
            // (the sole reader of the inherited stderr pipe) has exited.
            libc::dup2(pipe_write, libc::STDERR_FILENO);
            libc::close(pipe_write);
            libc::close(pipe_read);
        }

        // Without the fix, this write hits the reader-less pipe and fails with
        // EPIPE -- the exact error the daemon's `eprintln!` turns fatal.
        let before = raw_write(libc::STDERR_FILENO, b"stray daemon log\n");
        let before_epipe = matches!(
            before.as_ref().map_err(|e| e.raw_os_error()),
            Err(Some(code)) if code == libc::EPIPE
        );
        if !before_epipe {
            unsafe { libc::_exit(NO_EPIPE_BEFORE) };
        }

        // Apply the fix: fd 1 and fd 2 now point at /dev/null.
        detach_daemon_stdio();

        // The same write must now succeed -- the daemon can log forever without
        // ever EPIPE-ing once its launcher is gone.
        let after = raw_write(libc::STDERR_FILENO, b"stray daemon log\n");
        unsafe { libc::_exit(if after.is_ok() { OK } else { STILL_FAILS_AFTER }) };
    }

    // Parent: drop both pipe ends and reap the child.
    unsafe {
        libc::close(pipe_write);
        libc::close(pipe_read);
    }
    let mut status = 0i32;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, 0) },
        pid,
        "waitpid()"
    );

    assert!(
        libc::WIFEXITED(status),
        "child should exit normally, not die by signal; raw status {status}"
    );
    let code = libc::WEXITSTATUS(status);
    assert_ne!(
        code, NO_EPIPE_BEFORE,
        "precondition failed: a write to the reader-less stderr pipe did not EPIPE"
    );
    assert_ne!(
        code, STILL_FAILS_AFTER,
        "after detach, stderr writes must succeed (fd -> /dev/null) but still failed"
    );
    assert_eq!(
        code, OK,
        "daemon with stdio detached must survive the post-parent-exit stderr write"
    );
}
