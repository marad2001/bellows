//! Issue #194: which Bollard errors count as a *transport* failure of
//! the connection to the Docker daemon, and are therefore safe to
//! retry, versus which are a verdict the daemon (or a container) has
//! already delivered and must be surfaced as-is.
//!
//! The predicate is exercised here, from outside the crate, because
//! getting it wrong is the expensive mistake in both directions: too
//! narrow and a dropped connection still aborts a whole run; too wide
//! and bellows silently re-runs gates and re-bills agent phases.

use bellows::sandbox::is_transport_failure;
use bollard::errors::Error as BollardError;

#[test]
fn connection_drop_io_error_is_transport_shaped() {
    // The exact shape seen on seven aborted runs in the 2026-07-25 →
    // 2026-07-28 window:
    //   Sandbox(Bollard(IOError { err: Custom { kind: Other,
    //     error: "error reading a body from connection" } }))
    let err = BollardError::IOError {
        err: std::io::Error::other("error reading a body from connection"),
    };
    assert!(is_transport_failure(&err), "{err:?} must be retryable");
}

#[test]
fn request_timeout_is_transport_shaped() {
    // The eighth abort in the same window surfaced as bollard's
    // request-timeout error: the daemon never answered at all.
    assert!(is_transport_failure(&BollardError::RequestTimeoutError));
}

#[test]
fn broken_pipe_to_the_daemon_socket_is_transport_shaped() {
    // The other half of a dropped unix-socket connection: bellows is
    // the writer when the daemon goes away mid-request.
    let err = BollardError::IOError {
        err: std::io::Error::from(std::io::ErrorKind::BrokenPipe),
    };
    assert!(is_transport_failure(&err));
}

#[test]
fn non_zero_container_exit_is_never_transport_shaped() {
    // THE distinction this predicate exists to draw. A container that
    // started and exited non-zero is a verdict about the code under
    // test — bollard reports it through DockerContainerWaitError, and
    // retrying it would re-run the cargo gate and re-bill the agent
    // phase that produced it.
    let err = BollardError::DockerContainerWaitError {
        error: "container exited".to_string(),
        code: 1,
    };
    assert!(
        !is_transport_failure(&err),
        "a non-zero container exit must never be retried",
    );

    // Including the SIGKILL code the wall-clock deadline produces.
    let killed = BollardError::DockerContainerWaitError {
        error: String::new(),
        code: 137,
    };
    assert!(!is_transport_failure(&killed));
}

#[test]
fn daemon_answered_errors_are_not_transport_shaped() {
    // The daemon received the request and replied. Whatever it said,
    // the connection worked — re-sending would get the same answer.
    for status_code in [404u16, 409, 500] {
        let err = BollardError::DockerResponseServerError {
            status_code,
            message: "no such container".to_string(),
        };
        assert!(
            !is_transport_failure(&err),
            "status {status_code} is a daemon answer, not a transport failure",
        );
    }
}

#[test]
fn malformed_daemon_json_is_not_transport_shaped() {
    // A response arrived and failed to parse: the transport was fine,
    // and a retry would just re-fetch the same unparseable body.
    let err = BollardError::JsonDataError {
        message: "invalid type".to_string(),
        column: 12,
    };
    assert!(!is_transport_failure(&err));
}
