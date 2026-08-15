//! What this machine is supposed to run, declared ONCE.
//!
//! Before this, "which services exist" was written down in four places that had to agree by hand:
//! `doctor`'s table of labels and probes, the launchd plists, `scripts/install-bins.sh`'s map of
//! which binary belongs at which path, and `service.rs`'s label constant. Each of those got edited
//! separately on 2026-08-08..09 — the drift row, the restart-after-publish fix, the meeting-daemon
//! ownership change — and three copies that must agree is the shape every stale entry in this
//! project started as.
//!
//! **This declares intent; it is not what the machine obeys.** The plists are that, and
//! `install-bins.sh` still derives destination paths from them, deliberately. The value of having
//! both is that they can be COMPARED: a job installed here that nothing declares, or a declared
//! service whose plist runs another binary, is now a finding rather than a surprise
//! (`docs/specs/service-liveness.md`).

/// How to ask a service whether it is doing its job.
#[derive(Clone, Copy)]
pub enum Probe {
    /// A plain GET whose status is the answer.
    Get(&'static str),
    /// An MCP `initialize` over HTTP. The proxy answers 404 to every path but `/mcp` and 406 to a
    /// GET without the streaming `Accept`, so a "does the port respond" probe reports a healthy
    /// server as broken — measured on the first live run of this check. Speaking its protocol is
    /// the only probe that means anything.
    McpInitialize(&'static str),
    /// Nothing to ask: the bridges talk outward to Telegram, the pools hold a socket to the
    /// daemon. Reported as `skip`, because inventing a probe that cannot fail is worse than
    /// admitting there is none (`docs/specs/service-liveness.md`).
    None,
}

/// How a job is expected to be found.
#[derive(Clone, Copy, PartialEq)]
pub enum Shape {
    /// A service: it should be RUNNING. Not running is the failure this check exists for.
    Resident,
    /// A `StartInterval` job: between ticks it is correctly NOT running, and "no pid" says
    /// nothing. What matters is that it RAN recently — measured by the state file it writes on
    /// every run. Without this the watcher could not be watched: adding `com.rozum.doctor` to the
    /// resident list would have made it permanently, wrongly red.
    Periodic { every_secs: u64 },
}

/// Who actually serves, when that can be established independently of the job.
#[derive(Clone, Copy)]
pub enum Owner {
    /// The holder of the lock beside the unix socket (`docs/specs/meeting-socket-ownership.md`).
    /// A launchd job can be alive and WAITING while a client-spawned daemon holds this and serves —
    /// observed on the host 2026-08-05: job pid 42206, lock and listener on 42132.
    SocketLock,
    /// Nothing to ask: the job's own process is the server, or there is no server.
    JobItself,
}

/// The gateway's label, named once because two modules need it: the registry below and
/// `service.rs`, which generates that job's plist. It was written out in both until 2026-08-09.
pub const GATEWAY_LABEL: &str = "com.rozum.gateway";

/// One service: what it is called, what runs it, and how to tell whether it is doing its job.
pub struct Service {
    /// The launchd label; also the plist's basename.
    pub label: &'static str,
    /// The `svc:*` row name in `doctor`. Prefixed on purpose: `rozum doctor` already had checks
    /// called `gateway` and `meeting-daemon`, and two rows with one name are two facts a lookup
    /// cannot tell apart.
    pub row: &'static str,
    /// The binary this job is expected to exec, by basename. What the plist actually names is the
    /// machine's business; a mismatch between the two is worth reporting.
    pub program: &'static str,
    pub probe: Probe,
    pub owner: Owner,
    pub shape: Shape,
    /// What it serves, in the terms a report should use.
    pub what: &'static str,
}

/// Every service this product installs.
pub const ALL: &[Service] = &[
    Service {
        label: GATEWAY_LABEL,
        row: "svc:gateway",
        program: "rozum-gateway",
        probe: Probe::Get("http://127.0.0.1:8089/v1/models"),
        owner: Owner::JobItself,
        shape: Shape::Resident,
        what: "the resident model",
    },
    Service {
        label: "com.rozum.ucc-control",
        row: "svc:ucc-control",
        program: "rozum-gateway",
        probe: Probe::Get("http://127.0.0.1:8411/control/auth/status"),
        owner: Owner::JobItself,
        shape: Shape::Resident,
        what: "the control plane",
    },
    Service {
        label: "com.rozum.meeting-daemon",
        row: "svc:meeting-daemon",
        program: "rozum-gateway",
        probe: Probe::Get("http://127.0.0.1:8401/rooms"),
        owner: Owner::SocketLock,
        shape: Shape::Resident,
        what: "meeting rooms over REST",
    },
    Service {
        label: "com.rozum.meeting-ssc",
        row: "svc:meeting-ssc",
        program: "rozum-meeting-ssc",
        probe: Probe::Get("http://127.0.0.1:8405/"),
        owner: Owner::JobItself,
        shape: Shape::Resident,
        what: "the meeting PWA",
    },
    Service {
        label: "com.rozum.ucc-ssc",
        row: "svc:ucc-ssc",
        program: "rozum-ucc-ssc",
        // NOT `/`, which answers 404 — the same trap the mcp-http probe fell into, where "does the
        // port respond" reported a healthy server as broken. This route answers 403 with its OWN
        // body (`{"error":"invalid or revoked token"}`), which proves the process parsed the
        // request and applied its own rules rather than merely holding a socket.
        probe: Probe::Get("http://127.0.0.1:8412/control/public/matrix/cell"),
        owner: Owner::JobItself,
        shape: Shape::Resident,
        what: "the .ssc public matrix routes",
    },
    Service {
        label: "com.rozum.mcp-http",
        row: "svc:mcp-http",
        program: "rozum-meet",
        probe: Probe::McpInitialize("http://127.0.0.1:8779/mcp"),
        owner: Owner::JobItself,
        shape: Shape::Resident,
        what: "MCP over HTTP",
    },
    Service {
        label: "com.rozum.telegram",
        row: "svc:telegram",
        program: "rozum-telegram-bridge.sh",
        probe: Probe::None,
        owner: Owner::JobItself,
        shape: Shape::Resident,
        what: "the Telegram bridge (private)",
    },
    Service {
        label: "com.rozum.telegram-groups",
        row: "svc:telegram-groups",
        program: "rozum-telegram-groups-bridge.sh",
        probe: Probe::None,
        owner: Owner::JobItself,
        shape: Shape::Resident,
        what: "the Telegram bridge (groups)",
    },
    Service {
        label: "com.rozum.assistant",
        row: "svc:assistant",
        program: "rozum-gateway",
        probe: Probe::None,
        owner: Owner::JobItself,
        shape: Shape::Resident,
        what: "the participant pool",
    },
    Service {
        label: "com.rozum.assistant-groups",
        row: "svc:assistant-groups",
        program: "rozum-gateway",
        probe: Probe::None,
        owner: Owner::JobItself,
        shape: Shape::Resident,
        what: "the participant pool (groups)",
    },
    Service {
        label: "com.rozum.doctor",
        row: "svc:doctor",
        program: "rozum-gateway",
        probe: Probe::None,
        owner: Owner::JobItself,
        shape: Shape::Periodic { every_secs: 300 },
        what: "this liveness check itself",
    },
];

/// The declared service for a launchd label, if it is one of ours.
pub fn find(label: &str) -> Option<&'static Service> {
    ALL.iter().find(|s| s.label == label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is only worth having if it is complete: a service missing from it is invisible
    /// to every reader downstream.
    #[test]
    fn every_entry_is_distinct_and_named_consistently() {
        for s in ALL {
            assert!(s.label.starts_with("com.rozum."), "{} is not one of ours", s.label);
            assert_eq!(s.row, format!("svc:{}", s.label.trim_start_matches("com.rozum.")), "row must follow the label");
            assert!(!s.program.is_empty() && !s.what.is_empty(), "{} is under-declared", s.label);
        }
        let mut labels: Vec<&str> = ALL.iter().map(|s| s.label).collect();
        labels.sort_unstable();
        let n = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), n, "a duplicate label would make two services one row");
    }
}
