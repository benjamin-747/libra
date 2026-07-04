//! Fake-sink span assertion for `agent.rpc.invoke` (AG-18 /
//! `docs/development/tracing/agent.md` 落地执行补充规格 §6).
//!
//! Lives in its own integration-test binary: the assertion installs a
//! thread-local `tracing` subscriber, and tracing's per-callsite interest
//! cache can flap when sibling threads in the same process evaluate the
//! same callsites without a subscriber — a single-test binary removes
//! that concurrency by construction.

#![cfg(unix)]

use std::sync::Mutex;

use libra::internal::ai::observed_agents::{RpcAgent, RpcAgentBinary};

fn plant_script(dir: &std::path::Path, slug: &str, body: &str) -> RpcAgentBinary {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(format!("libra-agent-{slug}"));
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    RpcAgentBinary {
        slug: slug.to_string(),
        binary_path: path,
    }
}

/// `agent.rpc.invoke` span carries the required fields (slug, method,
/// protocol_version, timeout_ms, frame_bytes, terminal_state) and never
/// the response body (fake-sink assertion per agent.md 补充规格 §6).
#[test]
fn invoke_span_carries_required_fields_without_secrets() {
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(sink.clone())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .finish();

    let dir = tempfile::tempdir().unwrap();
    let body = "#!/bin/sh\n\
        read _l; printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"methods\":[\"capabilities\"],\"secret_payload\":\"DO-NOT-LOG-ME\"}}\\n'\n";
    let binary = plant_script(dir.path(), "spanprobe", body);

    tracing::subscriber::with_default(subscriber, || {
        let mut agent = RpcAgent::spawn(binary).unwrap();
        agent.negotiate_capabilities().unwrap();
    });

    let captured = String::from_utf8_lossy(&sink.0.lock().unwrap()).to_string();
    assert!(captured.contains("agent.rpc.invoke"), "{captured}");
    for field in [
        "slug=spanprobe",
        "method=capabilities",
        "protocol_version=1",
        "timeout_ms=",
        "terminal_state=\"ok\"",
        "frame_bytes=",
    ] {
        assert!(
            captured.contains(field),
            "span missing `{field}`: {captured}"
        );
    }
    assert!(
        !captured.contains("DO-NOT-LOG-ME"),
        "span must not carry the raw response body: {captured}"
    );
}

/// `agent.rpc.discover` events carry slug/external_binary/quarantined/
/// reason for both the quarantined-discovery and impersonation-skip
/// paths, and never raw env or stderr.
#[test]
fn discover_events_carry_required_fields() {
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(sink.clone())
        .finish();

    let dir = tempfile::tempdir().unwrap();
    plant_script(dir.path(), "probe", "#!/bin/sh\nexit 0\n");
    plant_script(dir.path(), "claude-code", "#!/bin/sh\nexit 0\n");

    let original = std::env::var_os("PATH");
    // SAFETY: single-test binary — no sibling test races this env var.
    unsafe {
        std::env::set_var("PATH", dir.path());
    }
    tracing::subscriber::with_default(subscriber, || {
        let _ = libra::internal::ai::observed_agents::discover_rpc_agents();
    });
    unsafe {
        match original {
            Some(prev) => std::env::set_var("PATH", prev),
            None => std::env::remove_var("PATH"),
        }
    }

    let captured = String::from_utf8_lossy(&sink.0.lock().unwrap()).to_string();
    for field in [
        "agent.rpc.discover",
        "slug=\"probe\"",
        "external_binary=true",
        "quarantined=true",
        "reason=\"discovered_untrusted_default\"",
        "slug=\"claude-code\"",
        "reason=\"builtin_slug_impersonation\"",
    ] {
        assert!(
            captured.contains(field),
            "discover event missing `{field}`: {captured}"
        );
    }
}

/// Failure paths also populate the required span fields: a timeout
/// records terminal_state="timeout" and frame_bytes=0.
#[test]
fn invoke_span_records_failure_terminal_state() {
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(sink.clone())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .finish();

    let dir = tempfile::tempdir().unwrap();
    let body = "#!/bin/sh\nread _l\n/bin/sleep 5\n";
    let binary = plant_script(dir.path(), "hangs", body);

    tracing::subscriber::with_default(subscriber, || {
        let mut agent = RpcAgent::spawn(binary).unwrap();
        let _ =
            agent.invoke_with_timeout("capabilities", None, std::time::Duration::from_millis(150));
    });

    let captured = String::from_utf8_lossy(&sink.0.lock().unwrap()).to_string();
    for field in ["terminal_state=\"timeout\"", "frame_bytes=0", "slug=hangs"] {
        assert!(
            captured.contains(field),
            "failure span missing `{field}`: {captured}"
        );
    }
}
