{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        crateName = "wol-mqtt";

        project = import ./Cargo.nix {
          inherit pkgs;
        };

      in {
        packages.${crateName} = project.rootCrate.build;

        defaultPackage = self.packages.${system}.${crateName};

        devShell = pkgs.mkShell {
          inputsFrom = builtins.attrValues self.packages.${system};
          buildInputs = [ pkgs.cargo pkgs.rust-analyzer pkgs.clippy ];
        };
      }) // {
    nixosModules.default = { lib, pkgs, config, ... }: with lib; 
    let
      cfg = config.services.wol-mqtt;
      wol-mqttBin = self.packages.${pkgs.stdenv.hostPlatform.system}.wol-mqtt;
    in {
      options.services.wol-mqtt = {
        enable = mkEnableOption "wol-mqtt";
        username = mkOption {
          type = types.nullOr types.str;
        };
        passwordFile = mkOption {
          type = types.nullOr types.path;
        };
        brokerIp = mkOption {
          type = types.str;
        };
        topicBase = mkOption {
          type = types.str;
        };
        sendInterval = mkOption {
          type = types.int;
        };
        timeout = mkOption {
          type = types.int;
        };
        devices = mkOption {
          type = types.attrsOf (types.submodule [{
            options = {
              ip = mkOption {
                type = types.str;
              };
            };
          }]);
        };
      };

      config = mkIf cfg.enable {
        systemd.services.wol-mqtt = let 
          config = {
            broker_ip = cfg.brokerIp;
            topic_base = cfg.topicBase;
            send_interval = cfg.sendInterval;
            timeout = cfg.timeout;
            devices = mapAttrs (name: dev: {
              ip = dev.ip;
            }) cfg.devices;
          };
          configFile = pkgs.writeTextFile {
            name = "wol-mqtt.yml";
            text = (lib.generators.toYAML { } config);
          };
        in 
        {
          wants = [ "basic.target" ];
          after = [ "basic.target" "network.target" ];
          wantedBy = [ "multi-user.target" ];
          serviceConfig = {
            Type = "simple";
            Restart = "always";
            RestartSec = "3";
            ExecStart = "${wol-mqttBin}/bin/wol-mqtt" + (if builtins.isString cfg.username then " --username=${escapeShellArg cfg.username}" else "") + (if builtins.isString cfg.passwordFile then " --password-file=${escapeShellArg cfg.passwordFile}" else "") + " " + configFile;
          };
        };
      };
    };
  };
}
