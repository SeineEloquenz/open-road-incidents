{ config, lib, pkgs, ... }:

let
  cfg = config.services.open-road-incidents;
  dataDir = "/var/lib/open-road-incidents";
  graphDir = "${dataDir}/valhalla";
  listen = "127.0.0.1:${toString cfg.valhalla.port}";

  jsonFormat = pkgs.formats.json { };
  valhallaOverrides = jsonFormat.generate "valhalla-overrides.json" cfg.valhalla.settings;

  valhallaConfig = pkgs.runCommand "valhalla.json" { nativeBuildInputs = [ cfg.valhalla.package pkgs.jq ]; } ''
    valhalla_build_config \
      --mjolnir-tile-extract ${graphDir}/valhalla_tiles.tar \
      --mjolnir-traffic-extract ${graphDir}/traffic.tar \
      --mjolnir-tile-dir ${graphDir}/tiles \
      --mjolnir-admin "" \
      --mjolnir-timezone "" \
      --httpd-service-listen tcp://${listen} \
      --httpd-service-loopback ipc://${graphDir}/loopback \
      --httpd-service-interrupt ipc://${graphDir}/interrupt \
      > base.json
    jq -s '.[0] * .[1]' base.json ${valhallaOverrides} > $out
  '';

  settings = {
    valhalla_url = "http://${listen}";
    traffic_extract = "${graphDir}/traffic.tar";
    user_agent = cfg.userAgent;
    out_dir = "${dataDir}/out";
    state_dir = "${dataDir}/state";
    source = cfg.sources;
  };
  configFile = (pkgs.formats.toml { }).generate "open-road-incidents.toml" settings;
in
{
  options.services.open-road-incidents = {
    enable = lib.mkEnableOption "open road incident collection with a Valhalla router";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The open-road-incidents package.";
    };

    userAgent = lib.mkOption {
      type = lib.types.str;
      default = "OpenRoadIncidents/0.1";
      description = "User-Agent sent to feed publishers.";
    };

    sources = lib.mkOption {
      type = lib.types.listOf (lib.types.attrsOf lib.types.anything);
      default = (builtins.fromTOML (builtins.readFile ../config.toml)).source;
      defaultText = lib.literalExpression "the sources in config.toml";
      description = "Feed sources, in the format of `[[source]]` entries in config.toml.";
    };

    valhalla = {
      package = lib.mkOption {
        type = lib.types.package;
        description = "The Valhalla package.";
      };
      port = lib.mkOption {
        type = lib.types.port;
        default = 8002;
        description = "Local port Valhalla listens on.";
      };
      threads = lib.mkOption {
        type = lib.types.ints.positive;
        default = 2;
        description = "Valhalla worker threads.";
      };
      settings = lib.mkOption {
        type = jsonFormat.type;
        default = { };
        example = { logging.color = false; };
        description = "Values merged over the generated valhalla.json. Valhalla has no log level, so `logging.type = \"\"` silences warnings and errors too.";
      };
      logRequests = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Log every request, its successful response and the per-request INFO lines. Disabling this keeps warnings, errors and failed responses.";
      };
    };

    graph = {
      enable = lib.mkEnableOption "keeping OSM extracts current and rebuilding the routing graph on this machine";
      package = lib.mkOption {
        type = lib.types.package;
        description = "The build-graph script.";
      };
      regions = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [
          "europe/finland"
          "europe/france"
          "europe/luxembourg"
          "europe/netherlands"
          "europe/belgium"
          "europe/spain"
          "europe/germany"
        ];
        description = "Geofabrik regions the graph covers.";
      };
      schedule = lib.mkOption {
        type = lib.types.str;
        default = "monthly";
        description = "When to update the extracts and rebuild, as a systemd calendar expression. Unchanged extracts skip the build.";
      };
      concurrency = lib.mkOption {
        type = lib.types.ints.positive;
        default = 2;
        description = "Build threads. Fewer threads need less memory.";
      };
      memoryMax = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "8G";
        description = "Memory limit for the build.";
      };
    };

    virtualHost = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Nginx virtual host serving the published files and Valhalla's route endpoint. Null disables nginx.";
    };

    routeRateLimit = lib.mkOption {
      type = lib.types.str;
      default = "2r/s";
      description = "Route requests allowed per client address.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.users.open-road-incidents = {
      isSystemUser = true;
      group = "open-road-incidents";
      home = dataDir;
    };
    users.groups.open-road-incidents = { };

    systemd.tmpfiles.settings.open-road-incidents = {
      ${dataDir}.d = { user = "open-road-incidents"; group = "open-road-incidents"; mode = "0755"; };
      ${graphDir}.d = { user = "open-road-incidents"; group = "open-road-incidents"; mode = "0755"; };
      "${graphDir}/incoming".d = { user = "open-road-incidents"; group = "open-road-incidents"; mode = "0755"; };
      "${dataDir}/graph".d = { user = "open-road-incidents"; group = "open-road-incidents"; mode = "0755"; };
    };

    systemd.services.valhalla = {
      description = "Valhalla routing";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      unitConfig.ConditionPathExists = [ "|${graphDir}/valhalla_tiles.tar" "|${graphDir}/incoming/valhalla_tiles.tar" ];
      serviceConfig = {
        User = "open-road-incidents";
        Group = "open-road-incidents";
        WorkingDirectory = graphDir;
        ExecStartPre = "+" + pkgs.writeShellScript "valhalla-swap-graph" ''
          cd ${graphDir}
          if [ -f incoming/valhalla_tiles.tar ] && [ -f incoming/traffic.tar ]; then
            mv incoming/valhalla_tiles.tar valhalla_tiles.tar
            mv incoming/traffic.tar traffic.tar
          fi
          chown open-road-incidents:open-road-incidents valhalla_tiles.tar traffic.tar
        '';
        ExecStart = "${cfg.valhalla.package}/bin/valhalla_service ${valhallaConfig} ${toString cfg.valhalla.threads}";
        Restart = "on-failure";
        RestartSec = 10;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ReadWritePaths = [ graphDir ];
        # prime_server's access log cannot be turned off in valhalla.json
        LogFilterPatterns = lib.mkIf (!cfg.valhalla.logRequests) [
          "~\\[INFO\\]"
          "~^[0-9]+ [0-9/]+ [0-9:.]+ (GET |POST |2[0-9]{2} )"
        ];
      };
    };

    systemd.services.open-road-incidents = {
      description = "Open road incident collection";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" "valhalla.service" ];
      wants = [ "network-online.target" ];
      requires = [ "valhalla.service" ];
      partOf = [ "valhalla.service" ];
      unitConfig.ConditionPathExists = [ "|${graphDir}/valhalla_tiles.tar" "|${graphDir}/incoming/valhalla_tiles.tar" ];
      environment.ORI_CONFIG = configFile;
      serviceConfig = {
        User = "open-road-incidents";
        Group = "open-road-incidents";
        WorkingDirectory = dataDir;
        ExecStart = "${lib.getExe cfg.package} run";
        Restart = "on-failure";
        RestartSec = 30;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ReadWritePaths = [ dataDir ];
      };
    };

    systemd.services.open-road-incidents-graph = lib.mkIf cfg.graph.enable {
      description = "Update OSM extracts and rebuild the Valhalla graph";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      environment.CONCURRENCY = toString cfg.graph.concurrency;
      serviceConfig = {
        Type = "oneshot";
        User = "open-road-incidents";
        Group = "open-road-incidents";
        ExecStart = "${lib.getExe cfg.graph.package} ${dataDir}/graph ${lib.escapeShellArgs cfg.graph.regions}";
        ExecStartPost = "+" + pkgs.writeShellScript "open-road-incidents-graph-install" ''
          ready=${dataDir}/graph/ready
          if [ -f $ready/valhalla_tiles.tar ] && [ -f $ready/traffic.tar ]; then
            mv $ready/valhalla_tiles.tar $ready/traffic.tar ${graphDir}/incoming/
            systemctl restart valhalla.service open-road-incidents.service
          fi
        '';
        TimeoutStartSec = "infinity";
        Nice = 19;
        CPUSchedulingPolicy = "batch";
        IOSchedulingClass = "idle";
        MemoryMax = lib.mkIf (cfg.graph.memoryMax != null) cfg.graph.memoryMax;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ReadWritePaths = [ dataDir ];
      };
    };

    systemd.timers.open-road-incidents-graph = lib.mkIf cfg.graph.enable {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = cfg.graph.schedule;
        OnBootSec = "15min";
        Persistent = true;
        RandomizedDelaySec = "1h";
      };
    };

    services.nginx = lib.mkIf (cfg.virtualHost != null) {
      enable = true;
      appendHttpConfig = ''
        limit_req_zone $binary_remote_addr zone=ori_route:10m rate=${cfg.routeRateLimit};
      '';
      virtualHosts.${cfg.virtualHost} = {
        locations."/v1/" = {
          root = "${dataDir}/out";
          extraConfig = ''
            types {
              application/geo+json geojson;
              application/json json;
              application/x-protobuf pbf;
            }
            gzip on;
            gzip_types application/geo+json application/json application/x-protobuf;
            add_header Cache-Control "public, max-age=60";
            add_header Access-Control-Allow-Origin "*";
          '';
        };
        locations."~ ^/(route|trace_attributes)$" = {
          proxyPass = "http://${listen}";
          extraConfig = ''
            limit_req zone=ori_route burst=10 nodelay;
            limit_req_status 429;
            client_max_body_size 64k;
            gzip on;
            gzip_types application/json;
          '';
        };
        locations."= /status".proxyPass = "http://${listen}";
      };
    };
  };
}
