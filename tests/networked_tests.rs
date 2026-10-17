//! The networked benchmark end to end: `origin_stub` and `dtac_standalone` on ephemeral
//! ports, and `replay_bench --target-url` against them, checked against an in-process
//! replay of the same records.

// The helpers below are test code too: a failed spawn or a missing address should panic.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Kills the server when the test ends, pass or fail.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `bin` with `args` and returns it with the address it printed on stdout.
fn start(bin: &str, args: &[&str]) -> (Server, String) {
    let mut child = Command::new(bin)
        .args(args)
        .env("RUST_LOG", "error")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let addr = lines
        .by_ref()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix("Listening on ").map(str::to_string))
        .expect("server exited without printing its address");
    // Keep draining stdout so the server never blocks on a full pipe.
    std::thread::spawn(move || lines.for_each(drop));
    (Server(child), addr)
}

/// 20 records: humans re-reading a few pages (so some are promoted and then hit), and a
/// bot scanning without headers from a datacenter-looking address.
fn write_log(path: &Path) {
    let mut lines = Vec::new();
    let human = r#"{"accept-language":"en-US","user-agent":"Mozilla/5.0"}"#;
    for i in 0..20_u32 {
        let (traffic, ip, headers, item, extra) = if i % 4 == 3 {
            (
                "bot",
                "45.33.12.4".to_string(),
                "{}",
                100 + i,
                r#""bot_kind":"naive""#,
            )
        } else {
            (
                "human",
                format!("24.0.0.{}", i % 5),
                human,
                i % 3,
                r#""visitor":"new""#,
            )
        };
        lines.push(format!(
            r#"{{"timestamp":{ts},"method":"GET","path":"/item/{item}","headers":{headers},"traffic_type":"{traffic}","ip":"{ip}","phase":"measurement","size_bytes":{size},{extra}}}"#,
            ts = f64::from(i) * 2.5,
            size = 512 + item,
        ));
    }
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

/// Runs `replay_bench` on `log` with `extra` arguments and returns its `--json-out`.
fn replay(log: &Path, out: &Path, extra: &[&str]) -> serde_json::Value {
    let status = Command::new(env!("CARGO_BIN_EXE_replay_bench"))
        .arg("--file")
        .arg(log)
        .arg("--json-out")
        .arg(out)
        .args(["--capacity", "1024", "--concurrency", "1", "--quiet"])
        .args(extra)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "replay_bench {extra:?} failed");
    serde_json::from_str(&std::fs::read_to_string(out).unwrap()).unwrap()
}

#[test]
fn networked_replay_matches_in_process_replay() {
    let dir = std::env::temp_dir().join(format!("dtac-networked-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("traffic.ndjson");
    write_log(&log);

    let (_origin, origin_addr) = start(
        env!("CARGO_BIN_EXE_origin_stub"),
        &["--port", "0", "--bind", "127.0.0.1"],
    );
    let target = format!("http://{origin_addr}");
    let (_proxy, proxy_addr) = start(
        env!("CARGO_BIN_EXE_dtac_standalone"),
        &[
            "--port",
            "0",
            "--bind",
            "127.0.0.1",
            "--capacity",
            "1024",
            "--target",
            &target,
            "--trusted-proxy",
            "127.0.0.1",
            "--clock",
            "replay-header",
            "--debug-headers",
        ],
    );

    let local = replay(&log, &dir.join("local.json"), &[]);
    let url = format!("http://{proxy_addr}");
    let networked = replay(&log, &dir.join("networked.json"), &["--target-url", &url]);

    for class in ["human", "bot", "overall"] {
        for field in ["requests", "hits", "misses", "bytes", "miss_bytes"] {
            assert_eq!(
                networked["results"][class][field], local["results"][class][field],
                "{class}.{field}"
            );
        }
    }
    let human_hits = local["results"]["human"]["hits"].as_u64().unwrap();
    assert!(human_hits > 0, "the log should produce some hits");
    assert_eq!(local["results"]["bot"]["hits"], 0);

    // The proxy's X-Bot-Probability gives the same classifier view as in process.
    let gap = |doc: &serde_json::Value| doc["results"]["classifier"]["mean_gap"].as_f64().unwrap();
    assert!((gap(&networked) - gap(&local)).abs() < 1e-6);
    assert_eq!(networked["run"]["target_url"], url.as_str());
    assert!(
        networked["results"]["pollution"].is_null(),
        "no CPI over HTTP"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
