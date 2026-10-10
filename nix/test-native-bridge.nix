{pkgs}: {
  package = ps:
    ps.buildPythonPackage {
      pname = "harbor-db-test-bridge";
      version = "1";
      src = ../python;
      format = "other";
      dontBuild = true;
      installPhase = ''
        mkdir -p "$out/${ps.python.sitePackages}"
        cp harbor_db/test_bridge.py "$out/${ps.python.sitePackages}/harbor_db_test_bridge.py"
      '';
    };

  # Transport only: each native fixture owns its assertions and acceptance file.
  script = {
    fixture,
    arguments ? [],
    nodes,
    artifact,
    artifactFlag ? "--acceptance",
  }: ''
    import os, socket, subprocess
    from harbor_db_test_bridge import serve
    control, inherited = socket.socketpair()
    fixture = subprocess.Popen(
        ${builtins.toJSON ([fixture] ++ arguments)} +
        ["--control-fd", str(inherited.fileno()),
         ${builtins.toJSON artifactFlag}, os.path.join(os.environ["out"], ${builtins.toJSON artifact})],
        pass_fds=(inherited.fileno(),))
    inherited.close()
    try:
        serve(control.fileno(), {${pkgs.lib.concatMapStringsSep ", " (name: "${builtins.toJSON name}: ${name}") nodes}})
    finally:
        control.close()
        if fixture.poll() is None:
            try:
                fixture.wait(timeout=30)
            except subprocess.TimeoutExpired:
                fixture.kill()
                fixture.wait(timeout=30)
    assert fixture.returncode == 0, fixture.returncode
  '';
}
