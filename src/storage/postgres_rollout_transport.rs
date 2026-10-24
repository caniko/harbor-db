//! Optional executor hand-off hook. From `prepared`, `rollout next` runs the
//! manifest's `executor.transport`, one immutable store executable, as the
//! transient root unit `harbor-db-rollout-transport.service`, with
//! `--config <opening manifest> --export <dir> --export-sha256 <hex> --receipt-dir <dir>`
//! appended. The unit starts in `/` with systemd's clean environment plus
//! `HOME=/root`, `LANG=C` and the caller's `SSH_AUTH_SOCK`; anything else the
//! hook needs (its `PATH` included) it sets itself. The hook copies the
//! manifest closure and the export to the executor, runs `rollout certify`
//! there, writes the receipt to `<receipt-dir>/off-host-recovery.json` (mode
//! 0600) and prints only the `receipt_sha256` certify printed on the executor.
//! Its stderr streams to the caller. Harbor imports the receipt exactly as
//! `rollout import` would; a failed or mismatched hand-off imports nothing.
use super::{
    Deferred, Journal, Rollout, TRANSPORT_UNIT, euid, read_pinned, sha_hex, verified_receipt,
};
use crate::storage::recovery::MAX_OFF_HOST_RECEIPT_BYTES;
use crate::storage::{Result, invalid};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ponytail: one fixed bound covering closure/export copy plus disposable
// replay; make it a manifest knob if a cluster's hand-off outgrows it.
const TIMEOUT: Duration = Duration::from_secs(8 * 3600);

/// A hook's verified receipt. Unknown fields are ignored, like the journal's.
#[derive(Clone, Deserialize, Serialize)]
pub struct Transported {
    pub receipt: PathBuf,
    pub sha256: String,
    /// The export digest the hook certified; another export (or none recorded)
    /// is certified again.
    #[serde(default)]
    pub export_sha256: String,
}

impl Rollout {
    fn attempts(&self) -> PathBuf {
        self.state.join("transport")
    }

    pub(super) fn transport(&self, journal: &mut Journal) -> Result<()> {
        let export_sha256 = (journal.export_sha256.clone())
            .ok_or_else(|| invalid("prepared rollout has no recorded export"))?;
        // A journaled hand-off resumes while it certifies this export, its
        // receipt is intact and no other receipt was recorded since.
        let resumable = journal.transported.clone().filter(|transported| {
            transported.export_sha256 == export_sha256
                && (journal.receipt_sha256.as_ref()).is_none_or(|sha| *sha == transported.sha256)
                && verified_receipt(&transported.receipt, &transported.sha256).is_ok()
        });
        let transported = match resumable {
            // `import` resumes itself: it reuses identical staged bytes and
            // recovers an admission that finished before it was journaled.
            Some(transported) => transported,
            // A recorded receipt is staged (its retained copy lost, or it was
            // imported by hand): retry its admission.
            None if self.import_slot(journal)? => return self.prepare(journal),
            None => {
                // Hours of copying only under this rollout's fence and evidence.
                self.inspect_fence(journal)?;
                self.require_bound_evidence(journal)?;
                let transported = self.hand_off(journal, &export_sha256)?;
                journal.transported = Some(transported.clone());
                self.save(journal)?;
                transported
            }
        };
        self.import(journal, &transported.receipt, &transported.sha256)?;
        // Staged and admitted: the attempt's copy is no longer needed.
        let _ = fs::remove_dir_all(self.attempts());
        Ok(())
    }

    /// Run the hook once, deferring termination signals so an interrupted
    /// coordinator stops its unit instead of orphaning it, and return its
    /// receipt held to import's preconditions and the executor's digest.
    fn hand_off(&self, journal: &Journal, export_sha256: &str) -> Result<Transported> {
        let hook = (self.manifest.executor.transport.as_ref())
            .ok_or_else(|| invalid("no executor transport hook is configured"))?;
        let export = (journal.export.as_ref())
            .ok_or_else(|| invalid("prepared rollout has no recorded export"))?;
        // One attempt at a time: an earlier failed or abandoned one goes first.
        match fs::remove_dir_all(self.attempts()) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let nanos = (SystemTime::now().duration_since(UNIX_EPOCH))
            .map_err(|_| invalid("clock precedes the epoch"))?
            .as_nanos();
        let attempt = self.attempts().join(nanos.to_string());
        self.walk(&attempt, &[], Some(0o700))?;
        let result = self.run_hook(hook, journal, export, export_sha256, &attempt);
        if result.is_err() {
            let _ = fs::remove_dir_all(&attempt);
        }
        result
    }

    fn run_hook(
        &self,
        hook: &Path,
        journal: &Journal,
        export: &Path,
        export_sha256: &str,
        attempt: &Path,
    ) -> Result<Transported> {
        let printed = attempt.join("stdout");
        let stdout = (fs::OpenOptions::new().write(true).create_new(true))
            .mode(0o600)
            .open(&printed)?;
        let mut command = Command::new(&self.manifest.systemd_run);
        command
            .args(["--quiet", "--wait", "--pipe", "--collect"])
            .arg(format!("--unit={TRANSPORT_UNIT}"))
            .args([
                "--property=Type=oneshot",
                "--property=KillMode=control-group",
                "--property=UMask=0077",
                "--working-directory=/",
                "--setenv=HOME=/root",
                "--setenv=LANG=C",
            ])
            .arg(format!("--property=TimeoutStartSec={}", TIMEOUT.as_secs()));
        if let Some(agent) = std::env::var_os("SSH_AUTH_SOCK") {
            let mut setenv = std::ffi::OsString::from("--setenv=SSH_AUTH_SOCK=");
            setenv.push(agent);
            command.arg(setenv);
        }
        // The GC-rooted opening manifest names the policies this window runs under.
        let config = journal.opening_config.as_ref().unwrap_or(&self.config);
        command
            .arg("--")
            .arg(hook)
            .arg("--config")
            .arg(config)
            .arg("--export")
            .arg(export)
            .args(["--export-sha256", export_sha256])
            .arg("--receipt-dir")
            .arg(attempt)
            .stdin(Stdio::null())
            .stdout(stdout);
        let signals = Deferred::block()?;
        if !self.await_unit(
            command,
            TRANSPORT_UNIT,
            TIMEOUT,
            "executor transport",
            &signals,
        )? {
            return Err(invalid(format!(
                "executor transport failed; nothing was imported (its output is above, or \
                 journalctl -u {TRANSPORT_UNIT})"
            )));
        }
        let printed = read_pinned(&printed, 4096)?.1;
        let sha256 = sha_hex(
            std::str::from_utf8(&printed)
                .map_err(|_| invalid("the transport hook printed no receipt SHA-256"))?
                .trim(),
        )?;
        let receipt = attempt.join("off-host-recovery.json");
        // Root's own file in a root-private attempt, still held to import's rules.
        if read_pinned(&receipt, MAX_OFF_HOST_RECEIPT_BYTES)?.0.uid() != euid() {
            return Err(invalid(
                "the transported receipt is not the coordinator's file",
            ));
        }
        verified_receipt(&receipt, &sha256)?;
        Ok(Transported {
            receipt,
            sha256,
            export_sha256: export_sha256.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fixture, executable, reported, store_tool};
    use super::super::{Action, Phase, load_journal};
    use super::*;
    use crate::storage::codec;
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt;

    const RECEIPT: &[u8] = br#"{"status":"ready"}"#;
    const NEXT: Action = Action::Next {
        adopt_fence_token: None,
    };

    /// A prepared rollout with a transport hook whose behaviour is `script`:
    /// the fake systemd-run runs it (a data file) in place of the store hook.
    fn prepared(f: &Fixture, script: &str) -> Rollout {
        let path = f.p("rollout.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["executor"]["transport"] = json!(store_tool());
        fs::write(&path, manifest.to_string()).unwrap();
        executable(
            &f.p("systemd-run"),
            &format!(
                r#"echo "systemd-run $*" >> {log}
case "$*" in
  *{TRANSPORT_UNIT}*) while [ "$1" != -- ]; do shift; done; shift 2; exec sh {hook} "$@" ;;
esac
exit 1"#,
                log = f.p("calls.log").display(),
                hook = f.p("hook").display(),
            ),
        );
        hook(f, script);
        let rollout = f.load(&path).unwrap();
        rollout
            .run(Action::Fence {
                adopt_fence_token: None,
            })
            .unwrap();
        reported(f, "backup-1");
        assert_eq!(rollout.run(Action::Prepare).unwrap()["phase"], "prepared");
        rollout
    }

    /// The hook logs its arguments, then runs `script` with `$dir` its receipt dir.
    fn hook(f: &Fixture, script: &str) {
        fs::write(
            f.p("hook"),
            format!(
                "echo \"$@\" >> {}\nfor dir; do :; done\n{script}",
                f.p("transport.log").display()
            ),
        )
        .unwrap();
    }

    /// Write `bytes` as the receipt (mode 0600) and print `digest`.
    fn certifying(bytes: &[u8], digest: &str) -> String {
        format!(
            "printf '%s' '{}' > \"$dir/off-host-recovery.json\"\nchmod 600 \"$dir/off-host-recovery.json\"\necho {digest}",
            String::from_utf8_lossy(bytes)
        )
    }

    /// Certify, and stage the receipt as `import` would (staging needs root):
    /// `import` then reuses the identical staged bytes.
    fn certifies(f: &Fixture) -> String {
        format!(
            "{}\nmkdir -p {import}; chmod 700 {import}\nprintf '%s' '{}' > {import}/receipt.json\nchmod 400 {import}/receipt.json",
            certifying(RECEIPT, &codec::digest(RECEIPT)),
            String::from_utf8_lossy(RECEIPT),
            import = f.p("import").display(),
        )
    }

    /// As `import` stages the receipt (staging itself needs root).
    fn staged(f: &Fixture) {
        fs::create_dir_all(f.p("import")).unwrap();
        fs::set_permissions(f.p("import"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(f.p("import/receipt.json"), RECEIPT).unwrap();
        fs::set_permissions(
            f.p("import/receipt.json"),
            fs::Permissions::from_mode(0o400),
        )
        .unwrap();
    }

    fn admitted(f: &Fixture) {
        fs::write(f.p("evidence/.admitted"), "").unwrap();
    }

    fn hook_runs(f: &Fixture) -> usize {
        fs::read_to_string(f.p("transport.log")).map_or(0, |log| log.lines().count())
    }

    #[test]
    fn transport_imports_the_receipt_the_executor_retained() {
        let f = Fixture::new(0);
        let rollout = prepared(&f, &certifies(&f));
        admitted(&f);
        let status = rollout.run(NEXT).unwrap();
        assert_eq!(status["phase"], "imported", "{status}");
        let journal = rollout.journal().unwrap().unwrap();
        assert_eq!(journal.receipt_sha256, Some(codec::digest(RECEIPT)));
        let plan = codec::file_digest(&f.p("export/backup-1/recovery-export.json")).unwrap();
        let args = fs::read_to_string(f.p("transport.log")).unwrap();
        let expected = format!(
            "--config {} --export {} --export-sha256 {plan} --receipt-dir {}/transport/",
            f.p("rollout.json").display(),
            f.p("export/backup-1").display(),
            f.p("state").display(),
        );
        assert!(args.starts_with(&expected), "{args}");
        // A deterministic transient unit from `/`, in systemd's clean environment.
        let calls = fs::read_to_string(f.p("calls.log")).unwrap();
        for flag in [
            "--unit=harbor-db-rollout-transport.service",
            "--working-directory=/",
            "--setenv=HOME=/root",
            "--property=KillMode=control-group",
        ] {
            assert!(calls.contains(flag), "missing {flag}");
        }
        // Imported: no attempt copy outlives the staged receipt.
        assert!(!f.p("state/transport").exists());
    }

    #[test]
    fn failed_or_mismatched_transport_imports_nothing() {
        let f = Fixture::new(0);
        let rollout = prepared(&f, "echo copy refused >&2; exit 1");
        let error = rollout.run(NEXT).unwrap_err().to_string();
        assert!(error.contains("transport failed"), "{error}");
        // A digest the executor did not retain is refused like `rollout import`.
        hook(&f, &certifying(RECEIPT, &"0".repeat(64)));
        let error = rollout.run(NEXT).unwrap_err().to_string();
        assert!(error.contains("SHA-256"), "{error}");
        let journal = rollout.journal().unwrap().unwrap();
        assert_eq!(journal.phase, Phase::Prepared);
        assert!(journal.receipt_sha256.is_none() && journal.transported.is_none());
        assert!(!f.p("import/receipt.json").exists());
        // Failed attempts are not kept.
        assert_eq!(fs::read_dir(f.p("state/transport")).unwrap().count(), 0);
    }

    #[test]
    fn a_receipt_failing_import_preconditions_is_never_journaled() {
        let f = Fixture::new(0);
        let receipt = certifying(RECEIPT, &codec::digest(RECEIPT));
        let rollout = prepared(&f, &receipt.replace("chmod 600", "chmod 644"));
        let error = rollout.run(NEXT).unwrap_err().to_string();
        assert!(error.contains("mode-0600"), "{error}");
        assert!(rollout.journal().unwrap().unwrap().transported.is_none());
        // A digest-matching receipt that is not a JSON object is refused too.
        hook(&f, &certifying(b"x", &codec::digest(b"x")));
        assert!(rollout.run(NEXT).is_err());
        assert!(rollout.journal().unwrap().unwrap().transported.is_none());
    }

    #[test]
    fn an_import_interrupted_while_staging_resumes_the_import_not_preparation() {
        let f = Fixture::new(0);
        let rollout = prepared(&f, &certifying(RECEIPT, &codec::digest(RECEIPT)));
        // Unprivileged staging fails after import recorded the digest.
        let error = rollout.run(NEXT).unwrap_err().to_string();
        assert!(error.contains("requires root"), "{error}");
        let journal = rollout.journal().unwrap().unwrap();
        assert_eq!(journal.receipt_sha256, Some(codec::digest(RECEIPT)));
        assert!(journal.transported.is_some());
        // The retry stages again rather than settling back to `prepared`.
        hook(&f, "exit 1");
        let error = rollout.run(NEXT).unwrap_err().to_string();
        assert!(error.contains("requires root"), "{error}");
        staged(&f);
        admitted(&f);
        assert_eq!(rollout.run(NEXT).unwrap()["phase"], "imported");
        assert_eq!(hook_runs(&f), 1, "the hook certified again");
    }

    #[test]
    fn a_staged_receipt_whose_copy_is_lost_is_readmitted_without_certifying() {
        let f = Fixture::new(0);
        let rollout = prepared(&f, &certifies(&f));
        // Not admitted the first time.
        assert!(rollout.run(NEXT).is_err());
        let journal = rollout.journal().unwrap().unwrap();
        let transported = journal.transported.unwrap();
        fs::remove_file(&transported.receipt).unwrap();
        hook(&f, "exit 1");
        fs::remove_file(f.p("transport.log")).unwrap();
        admitted(&f);
        assert_eq!(rollout.run(NEXT).unwrap()["phase"], "imported");
        assert_eq!(hook_runs(&f), 0);
    }

    #[test]
    fn a_hand_off_of_another_export_is_certified_again() {
        let f = Fixture::new(0);
        let rollout = prepared(&f, &certifies(&f));
        let mut journal = rollout.journal().unwrap().unwrap();
        let retained = f.p("retained.json");
        fs::write(&retained, RECEIPT).unwrap();
        fs::set_permissions(&retained, fs::Permissions::from_mode(0o600)).unwrap();
        journal.transported = Some(Transported {
            receipt: retained,
            sha256: codec::digest(RECEIPT),
            export_sha256: "e".repeat(64),
        });
        rollout.save(&journal).unwrap();
        admitted(&f);
        assert_eq!(rollout.run(NEXT).unwrap()["phase"], "imported");
        assert_eq!(hook_runs(&f), 1, "a receipt for another export was resumed");
        let journal = rollout.journal().unwrap().unwrap();
        assert_eq!(
            journal.transported.unwrap().export_sha256,
            journal.export_sha256.unwrap()
        );
    }

    #[test]
    fn the_hook_runs_only_under_this_rollouts_bound_evidence() {
        let f = Fixture::new(0);
        let rollout = prepared(&f, &certifies(&f));
        fs::write(
            f.p("evidence/records.json"),
            json!({"backup_id": "backup-1", "version": 1, "writer_fence_token": "f".repeat(32)})
                .to_string(),
        )
        .unwrap();
        let error = rollout.run(NEXT).unwrap_err().to_string();
        assert!(error.contains("another writer window"), "{error}");
        assert_eq!(hook_runs(&f), 0);
    }

    #[test]
    fn a_running_transport_unit_blocks_every_step_and_shows_in_status() {
        let f = Fixture::new(0);
        let rollout = prepared(&f, &certifies(&f));
        fs::write(f.p("units").join(TRANSPORT_UNIT), "active").unwrap();
        for action in [
            NEXT,
            Action::Abort {
                adopt_fence_token: None,
                adopt_startup_token: None,
            },
        ] {
            let error = rollout.run(action).unwrap_err().to_string();
            assert!(error.contains("transport hook"), "{error}");
        }
        assert_eq!(rollout.status().unwrap()["transport_hook"], TRANSPORT_UNIT);
        assert_eq!(hook_runs(&f), 0);
    }

    #[test]
    fn a_refused_receipt_points_at_manual_certification() {
        let f = Fixture::new(0);
        let rollout = prepared(&f, &certifies(&f));
        assert!(rollout.run(NEXT).is_err());
        let next = rollout.status().unwrap()["next"].to_string();
        assert!(
            next.contains("rollout certify") && next.contains("aside"),
            "{next}"
        );
    }

    #[test]
    fn transport_hooks_are_store_executables_and_journals_stay_lenient() {
        let f = Fixture::new(0);
        let path = f.p("rollout.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for hook in [json!(f.p("hook")), json!("transport")] {
            manifest["executor"]["transport"] = hook;
            fs::write(&path, manifest.to_string()).unwrap();
            let error = f.load(&path).err().unwrap().to_string();
            assert!(error.contains("transport hook"), "{error}");
        }
        // No hook: the journal never carries the key; a newer `transported`
        // with fields this coordinator does not know still loads.
        let rollout = prepared(&f, "exit 1");
        let mut journal = serde_json::to_value(rollout.journal().unwrap().unwrap()).unwrap();
        assert!(journal.get("transported").is_none(), "{journal}");
        journal["transported"] =
            json!({"receipt": "/r", "sha256": "a", "export_sha256": "b", "newer": 1});
        let saved = f.p("journal.json");
        fs::write(&saved, journal.to_string()).unwrap();
        assert!(load_journal(&saved).unwrap().unwrap().transported.is_some());
    }
}
