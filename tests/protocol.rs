#![cfg(feature = "testing")]
use harbor_db::testing::protocol::{Client, Request, Response, VERSION};
use serde_json::json;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    thread,
    time::Duration,
};

#[test]
fn control_transport_correlates_requests_and_keeps_logs_out_of_messages() {
    let (client, mut bridge) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        let mut reader = BufReader::new(bridge.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["request"]["operation"], "execute");
        assert_eq!(value["request"]["argv"][1], "argument with spaces");
        writeln!(
            bridge,
            "{}",
            serde_json::to_string(&Response {
                version: VERSION,
                id: value["id"].as_u64().unwrap(),
                result: Some(json!({"exit_code": 0, "output": "acknowledged"})),
                error: None
            })
            .unwrap()
        )
        .unwrap();
    });
    let mut client = Client::new(client, Duration::from_secs(2)).unwrap();
    let result = client
        .call(Request::execute(
            "primary",
            vec!["printf".into(), "argument with spaces".into()],
        ))
        .unwrap();
    assert_eq!(result["output"], "acknowledged");
    worker.join().unwrap();
}

#[test]
fn unknown_versions_wrong_ids_and_ambiguous_results_are_rejected() {
    for response in [
        json!({"version": 2, "id": 1, "result": {}}),
        json!({"version": 1, "id": 42, "result": {}}),
        json!({"version": 1, "id": 1, "result": {}, "error": {"kind":"failure", "message":"bad"}}),
    ] {
        let (client, mut bridge) = UnixStream::pair().unwrap();
        let worker = thread::spawn(move || {
            let mut input = String::new();
            BufReader::new(bridge.try_clone().unwrap())
                .read_line(&mut input)
                .unwrap();
            writeln!(bridge, "{response}").unwrap();
        });
        assert!(
            Client::new(client, Duration::from_secs(1))
                .unwrap()
                .call(Request::Start {
                    node: "primary".into(),
                    allow_reboot: false
                })
                .is_err()
        );
        worker.join().unwrap();
    }
}

#[test]
fn prototype_bridge_uses_the_same_inherited_transport_without_mixing_diagnostics() {
    use std::{
        os::{fd::AsRawFd, unix::process::CommandExt},
        process::Command,
    };
    let (stream, bridge) = UnixStream::pair().unwrap();
    let fd = bridge.as_raw_fd();
    let mut command = Command::new("python3");
    command
        .args([
            "-B",
            "-m",
            "harbor_db.test_bridge",
            "--control-fd",
            &fd.to_string(),
        ])
        .env(
            "PYTHONPATH",
            format!("{}/python", env!("CARGO_MANIFEST_DIR")),
        );
    // SAFETY: the live socket descriptor is inherited by this child only.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(bridge);
    let mut client = Client::new(stream, Duration::from_secs(10)).unwrap();
    let result = client
        .call(Request::execute(
            "local",
            vec![
                "python3".into(),
                "-c".into(),
                "print('one argument é')".into(),
            ],
        ))
        .unwrap();
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["output"], "one argument é\n");
    drop(client);
    assert!(child.wait().unwrap().success());
}

#[test]
fn transport_deadline_bounds_a_bridge_that_never_answers() {
    let (stream, _bridge) = UnixStream::pair().unwrap();
    let mut client = Client::new(stream, Duration::from_millis(25)).unwrap();
    assert!(
        client
            .call(Request::Stop {
                node: "primary".into()
            })
            .is_err()
    );
    assert!(
        client
            .call(Request::Stop {
                node: "primary".into()
            })
            .unwrap_err()
            .to_string()
            .contains("reconnection")
    );
}

#[test]
fn driver_directory_export_is_normalized_to_the_exact_protocol_filename() {
    let root = tempfile::tempdir().unwrap();
    let script = r#"import pathlib,sys
from harbor_db.test_bridge import dispatch
root=pathlib.Path(sys.argv[1])
payload=bytes(range(256))+b'\x00acknowledged\xff'
class Driver:
    def copy_from_machine(self, source, target_dir):
        # The pinned NixOS driver appends the guest basename to target_dir.
        target=pathlib.Path(target_dir)/pathlib.Path(source).name
        target.parent.mkdir(parents=True,exist_ok=True)
        target.write_bytes(payload)
    def copy_from_host(self, source, destination):
        assert pathlib.Path(source).is_file()
        assert pathlib.Path(source).read_bytes()==payload
        assert destination=='/run/receipt.json'
destination=root/'different-host-name.bin'
driver=Driver()
assert dispatch({'operation':'copy_from','node':'guest','source':'/run/export.tar','destination':str(destination)},{'guest':driver})=={'transferred':True}
assert destination.is_file() and destination.read_bytes()==payload
assert sorted(p.name for p in root.iterdir())==['different-host-name.bin']
assert dispatch({'operation':'copy_to','node':'guest','source':str(destination),'destination':'/run/receipt.json'},{'guest':driver})=={'transferred':True}
"#;
    let mut command = std::process::Command::new("python3");
    command.args(["-B", "-c", script]).arg(root.path()).env(
        "PYTHONPATH",
        format!("{}/python", env!("CARGO_MANIFEST_DIR")),
    );
    let output = harbor_db::storage::process::spawn(&mut command)
        .unwrap()
        .wait()
        .unwrap();
    assert!(output.success(), "bridge filename normalization failed");
}
