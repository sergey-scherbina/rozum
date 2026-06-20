use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use rozum::sandbox::{
    DockerLimits, NetPolicy, SandboxPolicy, autonomy_flag_for, default_docker_image,
    write_seatbelt_profile_temp,
};

fn v(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

fn has_window(args: &[String], needle: &[&str]) -> bool {
    args.windows(needle.len())
        .any(|w| w.iter().zip(needle).all(|(a, b)| a == b))
}

#[test]
fn seatbelt_profile_preserves_core_jail_invariants() {
    let policy = SandboxPolicy {
        writable: vec![PathBuf::from("/private/tmp/rozum-regression-ws")],
        read_only: vec![],
        secret_deny: vec![PathBuf::from("/private/tmp/rozum-regression-ws/.ssh")],
        network: NetPolicy::GatewayStrict,
    };
    let profile = policy.to_seatbelt_profile();
    assert!(profile.starts_with("(version 1)\n(deny default)\n"));
    assert!(profile.contains("(allow file-read*)\n"));
    assert!(profile.contains("(subpath \"/private/tmp/rozum-regression-ws\")"));
    assert!(
        profile.contains("(deny file-read* (subpath \"/private/tmp/rozum-regression-ws/.ssh\"))")
    );
    assert!(
        profile.contains("(deny file-write* (subpath \"/private/tmp/rozum-regression-ws/.ssh\"))")
    );
    let allow_write = profile.find("(allow file-write*").unwrap();
    let deny_secret = profile
        .find("(deny file-write* (subpath \"/private/tmp/rozum-regression-ws/.ssh\"))")
        .unwrap();
    assert!(
        deny_secret > allow_write,
        "secret deny must override writable workspace"
    );
    assert!(profile.contains("(allow network* (local ip) (remote ip \"localhost:*\"))"));
    assert!(!profile.contains("(allow network*)\n"));
}

#[test]
fn docker_argv_preserves_core_jail_invariants() {
    let policy = SandboxPolicy {
        writable: vec![PathBuf::from("/tmp/rozum-regression-ws")],
        read_only: vec![PathBuf::from("/tmp/rozum-regression-ref")],
        secret_deny: vec![PathBuf::from("/tmp/rozum-regression-ws/.ssh")],
        network: NetPolicy::GatewayStrict,
    };
    let limits = DockerLimits {
        memory: Some("512m".into()),
        cpus: Some("2".into()),
        pids: Some(128),
    };
    let args = policy.to_docker_run_args(
        "rozum-agent:test",
        Path::new("/tmp/rozum-regression-ws"),
        &["OPENAI_BASE_URL", "OPENCODE_CONFIG"],
        &limits,
    );

    assert_eq!(&args[0..4], &["run", "--rm", "-i", "--init"]);
    assert!(has_window(&args, &["--memory", "512m"]));
    assert!(has_window(&args, &["--cpus", "2"]));
    assert!(has_window(&args, &["--pids-limit", "128"]));
    assert!(has_window(
        &args,
        &["-v", "/tmp/rozum-regression-ws:/tmp/rozum-regression-ws:rw"]
    ));
    assert!(has_window(
        &args,
        &[
            "-v",
            "/tmp/rozum-regression-ref:/tmp/rozum-regression-ref:ro"
        ]
    ));
    assert!(has_window(
        &args,
        &["--tmpfs", "/tmp/rozum-regression-ws/.ssh"]
    ));
    assert!(has_window(
        &args,
        &["--add-host", "host.docker.internal:host-gateway"]
    ));
    assert!(args.iter().any(|a| a == "--cap-add=NET_ADMIN"));
    assert!(has_window(&args, &["-e", "ROZUM_EGRESS=strict"]));
    assert!(has_window(&args, &["-e", "OPENAI_BASE_URL"]));
    assert!(has_window(&args, &["-e", "OPENCODE_CONFIG"]));
    assert_eq!(args.last().unwrap(), "rozum-agent:test");
}

#[test]
fn docker_none_network_has_no_gateway_alias_or_strict_marker() {
    let policy = SandboxPolicy {
        writable: vec![PathBuf::from("/tmp/rozum-regression-ws")],
        read_only: vec![],
        secret_deny: vec![],
        network: NetPolicy::None,
    };
    let args = policy.to_docker_run_args(
        "img",
        Path::new("/tmp/rozum-regression-ws"),
        &[],
        &DockerLimits::none(),
    );
    assert!(args.iter().any(|a| a == "--network=none"));
    assert!(!args.iter().any(|a| a == "--add-host"));
    assert!(!args.iter().any(|a| a == "ROZUM_EGRESS=strict"));
}

#[test]
fn opencode_config_forwarding_stays_docker_visible_when_under_tmp() {
    let policy = SandboxPolicy::rust_coding(
        &[PathBuf::from("/tmp/rozum-regression-ws")],
        NetPolicy::GatewayOnly,
    );
    let args = policy.to_docker_run_args(
        "img",
        Path::new("/tmp/rozum-regression-ws"),
        &["OPENCODE_CONFIG"],
        &DockerLimits::none(),
    );
    assert!(has_window(&args, &["-e", "OPENCODE_CONFIG"]));
    assert!(
        args.iter()
            .any(|a| a == "/tmp:/tmp:rw" || a == "/private/tmp:/private/tmp:rw"),
        "rust_coding must mount conventional tmp so OPENCODE_CONFIG under /tmp is visible"
    );
}

#[test]
fn autonomy_flags_only_apply_to_jailed_headless_agents() {
    assert_eq!(autonomy_flag_for(&v(&["claude", "-p", "hi"]), false), None);
    assert_eq!(
        autonomy_flag_for(&v(&["claude", "-p", "hi"]), true),
        Some("--dangerously-skip-permissions")
    );
    assert_eq!(autonomy_flag_for(&v(&["claude"]), true), None);
    assert_eq!(
        autonomy_flag_for(&v(&["codex", "exec", "hi"]), true),
        Some("--dangerously-bypass-approvals-and-sandbox")
    );
    assert_eq!(
        autonomy_flag_for(&v(&["codex", "exec", "hi", "-s", "workspace-write"]), true),
        None
    );
    assert_eq!(
        autonomy_flag_for(&v(&["opencode", "run", "hi"]), true),
        Some("--dangerously-skip-permissions")
    );
    assert_eq!(autonomy_flag_for(&v(&["opencode"]), true), None);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "runs sandbox-exec and writes temp files; macOS only"]
fn seatbelt_e2e_allows_workspace_and_denies_secret_and_escape() {
    let id = std::process::id();
    let ws = std::env::temp_dir().join(format!("rozum-seatbelt-regression-{id}"));
    let secret = ws.join(".ssh");
    let secret_file = secret.join("id_rsa");
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(&secret_file, "TOP-SECRET").unwrap();

    let policy =
        SandboxPolicy::rust_coding_with(&[ws.clone()], &[], &[secret.clone()], NetPolicy::None);
    let profile = write_seatbelt_profile_temp(&policy).unwrap();
    let inside = ws.join("inside.txt");
    let write_inside = Command::new("sandbox-exec")
        .arg("-f")
        .arg(&profile)
        .args(["/bin/sh", "-c", &format!("echo ok > {}", inside.display())])
        .status()
        .expect("spawn sandbox-exec write inside");
    assert!(write_inside.success(), "workspace write must be allowed");
    assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "ok");

    let read_secret = Command::new("sandbox-exec")
        .arg("-f")
        .arg(&profile)
        .args(["/bin/cat", secret_file.to_str().unwrap()])
        .output()
        .expect("spawn sandbox-exec read secret");
    assert!(!read_secret.status.success(), "secret read must be denied");

    let write_secret = Command::new("sandbox-exec")
        .arg("-f")
        .arg(&profile)
        .args([
            "/bin/sh",
            "-c",
            &format!("echo leaked > {}", secret_file.display()),
        ])
        .status()
        .expect("spawn sandbox-exec write secret");
    assert!(!write_secret.success(), "secret write must be denied");
    assert_eq!(std::fs::read_to_string(&secret_file).unwrap(), "TOP-SECRET");

    let home = std::env::var("HOME").expect("HOME set");
    let escape = Path::new(&home).join(format!(".rozum-seatbelt-regression-{id}"));
    let _ = std::fs::remove_file(&escape);
    let _ = Command::new("sandbox-exec")
        .arg("-f")
        .arg(&profile)
        .args([
            "/bin/sh",
            "-c",
            &format!("echo escaped > {}", escape.display()),
        ])
        .status();
    let leaked = escape.exists();
    let _ = std::fs::remove_file(&escape);
    let _ = std::fs::remove_file(&profile);
    let _ = std::fs::remove_dir_all(&ws);
    assert!(!leaked, "sandbox escape: wrote outside the workspace");
}

#[test]
#[ignore = "runs docker; needs Docker daemon and the configured rozum-agent image"]
fn docker_e2e_gateway_strict_reaches_host_and_blocks_internet() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local http listener");
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept one docker request");
        let mut buf = [0_u8; 512];
        let _ = stream.read(&mut buf);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
    });

    let ws = std::env::temp_dir().join(format!("rozum-docker-net-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&ws);
    let image = default_docker_image();
    let policy = SandboxPolicy::rust_coding(&[ws.clone()], NetPolicy::GatewayStrict);
    let script = format!(
        "set -e; \
         wget -q -T 3 -O - http://host.docker.internal:{port}/ | grep -q ok; \
         if wget -q -T 3 -O - http://1.1.1.1/ >/tmp/egress.out 2>/tmp/egress.err; then exit 42; fi"
    );
    let out = Command::new("docker")
        .args(policy.to_docker_run_args(&image, &ws, &[], &DockerLimits::none()))
        .args(["bash", "-lc", &script])
        .output()
        .expect("spawn docker strict network test");
    let _ = server.join();
    let _ = std::fs::remove_dir_all(&ws);
    assert!(
        out.status.success(),
        "gateway-strict failed: status={:?}\nstdout={}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[ignore = "runs docker + cargo build; needs Docker daemon and the configured rozum-agent image"]
fn docker_e2e_builds_simple_crate_inside_jail() {
    let ws = std::env::temp_dir().join(format!("rozum-docker-build-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).unwrap();
    let image = default_docker_image();
    let policy = SandboxPolicy::rust_coding(&[ws.clone()], NetPolicy::None);
    let wsd = ws.to_string_lossy().into_owned();
    let out = Command::new("docker")
        .args(policy.to_docker_run_args(&image, &ws, &[], &DockerLimits::none()))
        .args([
            "bash",
            "-lc",
            &format!(
                "set -e; cd {wsd}; cargo new --bin demo >/dev/null 2>&1; cd demo; \
                 cargo build --offline >/dev/null 2>&1; ./target/debug/demo"
            ),
        ])
        .output()
        .expect("spawn docker build test");
    let binary_exists = ws.join("demo/target/debug/demo").exists();
    let _ = std::fs::remove_dir_all(&ws);
    assert!(
        out.status.success(),
        "cargo build failed in Docker jail:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Hello, world!"));
    assert!(
        binary_exists,
        "build output must round-trip to the host workspace"
    );
}
