{ inputs, ... }:
{
  imports = [ inputs.process-compose-flake.flakeModule ];

  perSystem =
    {
      pkgs,
      lib,
      ...
    }:
    {
      process-compose.dev =
        let
          opencodePort = toString 14252;
        in
        {
          options.environmentVariables = lib.mkOption {
            type = lib.types.attrsOf lib.types.str;
            default = { };
          };

          config = {
            environmentVariables = {
              OPENCODE_PORT = opencodePort;
            };

            settings.processes.opencode.command = "OPENCODE_ENABLE_EXA=1 nix run github:numtide/llm-agents.nix#opencode -- serve --port ${opencodePort}";

            settings.processes.webserver.command = "${lib.getExe pkgs.watchexec} -r -- cargo run -p nixsearch -- serve";

            cli.options.unix-socket = "process-compose-socket";
          };
        };
    };
}
