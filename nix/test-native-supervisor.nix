{
  pkgs,
  testPackage,
}:
assert pkgs.stdenv.hostPlatform.system == "x86_64-linux";
  pkgs.testers.runNixOSTest {
    name = "harbor-db-native-supervisor";
    extraDriverArgs = ["--junit-xml" "junit.xml"];
    nodes.machine = {
      virtualisation = {
        memorySize = 1024;
        cores = 2;
      };
      users.users.qualifier = {
        isNormalUser = true;
        uid = 1000;
        linger = true;
      };
      environment.systemPackages = [testPackage pkgs.systemd pkgs.coreutils pkgs.util-linux];
    };
    # Python is only VM/session transport. Rust owns every behavioral assertion,
    # deadline, semantic producer, systemd invocation and negative verdict check.
    testScript = ''
      start_all()
      machine.wait_for_unit("user@1000.service")
      machine.wait_until_succeeds("test -S /run/user/1000/bus")
      with subtest("native detached supervisor qualification v1"):
          machine.succeed(
              "runuser -u qualifier -- env "
              "XDG_RUNTIME_DIR=/run/user/1000 "
              "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus "
              "timeout --kill-after=5 240 "
              "${testPackage}/bin/harbor-db-native-supervisor-fixture qualify "
              "/home/qualifier/native-supervisor-v1 "
              "${testPackage}/bin/harbor-db-test",
              timeout=250,
          )
      machine.copy_from_machine("/home/qualifier/native-supervisor-v1", "retained-supervisor")
    '';
  }
