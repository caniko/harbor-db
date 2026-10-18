{
  pkgs,
  mkBtrfsCrashTest,
}:
mkBtrfsCrashTest {
  inherit pkgs;
  name = "harbor-db-btrfs-crash";
  node = {};
  testScript = ''
    import json
    import os
    import shlex
    from pathlib import Path

    root = storage_fixture.mount_point
    with subtest("synchronous mounts are rejected"):
        machine.succeed("mount -o remount,sync " + shlex.quote(root))
        try:
            storage_fixture.verify()
        except AssertionError:
            pass
        else:
            raise AssertionError("synchronous mount was accepted")
        machine.succeed("mount -o remount,async " + shlex.quote(root))
        storage_fixture.verify()

    with subtest("acknowledged bytes recover without a clean shutdown"):
        expected = "acknowledged revision seven: Küche"
        path = root + "/acknowledged"
        code = f"import os; f=open({path!r},'w'); f.write({expected!r}); f.flush(); os.fsync(f.fileno()); f.close(); d=os.open({root!r},os.O_RDONLY); os.fsync(d); os.close(d)"
        machine.succeed("python3 -c " + shlex.quote(code))

        def recovered():
            machine.wait_until_succeeds("mountpoint -q " + shlex.quote(root))
            assert machine.succeed("cat " + shlex.quote(path)) == expected

        storage_fixture.power_cut("acknowledged-file", recovered)
        receipt = storage_fixture.receipt()
        assert receipt["crashes"] == ["acknowledged-file"], receipt
        Path(os.environ["out"], "receipts.json").write_text(json.dumps(receipt, indent=2) + "\n")
  '';
}
