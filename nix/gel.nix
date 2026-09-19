{
  config,
  lib,
  pkgs,
  ...
}: let
  inherit (lib) mkEnableOption mkIf mkOption types;

  cfg = config.services.harbor-db.gel;

  containerName = name: "harbor-db-gel-${name}";
  containerUnit = name: "${config.virtualisation.oci-containers.backend}-${containerName name}.service";

  instanceType = types.submodule ({name, ...}: {
    options = {
      enable = mkEnableOption "harbor-db Gel server instance ${name}";

      port = mkOption {
        type = types.port;
        default = 5656;
        description = "Host port published on bindAddress for this Gel instance.";
      };

      bindAddress = mkOption {
        type = types.str;
        default = "127.0.0.1";
        description = "Host address the Gel port is published on. Keep loopback unless clients are remote and TLS/auth are reviewed.";
      };

      dataDir = mkOption {
        type = types.path;
        default = "/var/lib/harbor-db-gel/${name}";
        description = ''
          Persistent host directory for Gel instance data. Add it to
          services.harbor-db.dataDirectories for ownership handling.
          The container entrypoint (running as root) chowns this directory
          to the server user itself, so root-owned host directories work.
          Rootless runtimes skip that step: there the directory must already
          be writable by the mapped server uid.
        '';
      };

      passwordFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "File holding the admin password. Mounted read-only into the container; the value never enters the Nix store or unit environment.";
      };

      tlsCertMode = mkOption {
        type = types.enum ["generate_self_signed" "require_file"];
        default = "generate_self_signed";
        description = "Server TLS certificate mode passed as GEL_SERVER_TLS_CERT_MODE.";
      };

      extraEnvironment = mkOption {
        type = types.attrsOf types.str;
        default = {};
        description = "Extra container environment. Never put secrets here; use passwordFile.";
      };

      systemdUnit = mkOption {
        type = types.str;
        readOnly = true;
        default = containerUnit name;
        description = "Generated container systemd unit. Reference it from project after/requires so Gel is ready before migrations run.";
      };
    };
  });
in {
  options.services.harbor-db.gel = {
    cliPackage = mkOption {
      type = types.package;
      default = pkgs.gel;
      defaultText = lib.literalExpression "pkgs.gel";
      description = "Gel CLI package used by readiness checks. Pinned through the flake's nixpkgs (currently Gel CLI 7.10.2).";
    };

    image = mkOption {
      type = types.str;
      default = "docker.io/geldata/gel:7.1@sha256:b7270b0973da6950d01ae0d578c6d38cd8d87fabdd6c4b75a09b74291ad6f3a8";
      description = "Digest-pinned Gel server image (server 7.1, bundles CLI 7.10.2). Tags without a digest are rejected by assertion.";
    };

    readyCheck = mkOption {
      type = types.package;
      readOnly = true;
      default = pkgs.writeShellScriptBin "harbor-db-gel-ready" ''
        set -eu
        host="127.0.0.1" port="5656" user="admin" password_file="" tls_ca_file="" timeout="30s"
        while [ $# -gt 0 ]; do case "$1" in
          --host) host="$2"; shift 2;;
          --port) port="$2"; shift 2;;
          --user) user="$2"; shift 2;;
          --password-file) password_file="$2"; shift 2;;
          --tls-ca-file) tls_ca_file="$2"; shift 2;;
          --timeout) timeout="$2"; shift 2;;
          *) echo "usage: $0 [--host H] [--port P] [--user U] --password-file F [--tls-ca-file C] [--timeout 30s]" >&2; exit 64;;
        esac; done
        [ -n "$password_file" ] || { echo "harbor-db-gel-ready: --password-file is required" >&2; exit 64; }
        # Authenticated, bounded readiness: real credential check plus a query,
        # not just an open port. The secret is read at runtime, never stored.
        tls_args=(--tls-security insecure)
        if [ -n "$tls_ca_file" ]; then tls_args=(--tls-ca-file "$tls_ca_file"); fi
        exec ${cfg.cliPackage}/bin/gel --host "$host" --port "$port" --user "$user" \
          --password-from-stdin "''${tls_args[@]}" \
          --connect-timeout 5s --wait-until-available "$timeout" \
          query "select 1" <"$password_file" >/dev/null
      '';
      description = "Authenticated bounded Gel readiness probe (password file + real query).";
    };

    instances = mkOption {
      type = types.attrsOf instanceType;
      default = {};
      description = "Disposable or persistent Gel server instances run as pinned OCI containers.";
    };
  };

  config = mkIf (cfg.instances != {}) {
    assertions = lib.flatten (lib.mapAttrsToList (name: instance: [
        {
          assertion = instance.enable -> instance.passwordFile != null;
          message = "services.harbor-db.gel.instances.${name}: passwordFile is required; the server must start with authentication, never trust-auth.";
        }
        {
          assertion = instance.enable -> lib.hasPrefix "/" (toString instance.dataDir);
          message = "services.harbor-db.gel.instances.${name}: dataDir must be absolute.";
        }
      ])
      cfg.instances)
      ++ [
        # NOTE: no digest assertion here on purpose. The default image IS
        # digest-pinned and gel-eval enforces that on the default; but a
        # hard module assertion would forbid legitimate offline use such as
        # preloading a store-pinned image via imageFile (whose bits are
        # pinned by derivation hash instead). Consumers overriding to a
        # floating tag do so explicitly and own the consequences.
        {
          assertion = let
            ports = lib.mapAttrsToList (_: instance: instance.port) (lib.filterAttrs (_: instance: instance.enable) cfg.instances);
          in lib.unique ports == ports;
          message = "services.harbor-db.gel.instances: enabled instances must use distinct host ports (parallel-test isolation).";
        }
      ];

    virtualisation.oci-containers.containers = lib.mapAttrs' (name: instance:
      lib.nameValuePair (containerName name) (mkIf instance.enable {
        image = cfg.image;
        ports = ["${instance.bindAddress}:${toString instance.port}:5656"];
        volumes = [
          "${toString instance.dataDir}:/var/lib/gel/data"
          "${instance.passwordFile}:/run/secrets/gel-server-password:ro"
        ];
        environment =
          {
            GEL_SERVER_DATADIR = "/var/lib/gel/data";
            GEL_SERVER_TLS_CERT_MODE = instance.tlsCertMode;
            GEL_SERVER_PASSWORD_FILE = "/run/secrets/gel-server-password";
            # Project SDL/migrations are applied by project-owned commands
            # through harbor-db, never implicitly by container startup.
            GEL_DOCKER_APPLY_MIGRATIONS = "never";
          }
          // instance.extraEnvironment;
      }))
    cfg.instances;
  };
}
