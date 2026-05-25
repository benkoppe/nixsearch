{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      lib,
      self',
      ...
    }:
    let
      craneLib = inputs.crane.mkLib pkgs;
      cargoSourceFiles = lib.fileset.unions [
        (craneLib.fileset.commonCargoSources ../.)
        ../crates/web/style.css
      ];

      src = lib.fileset.toSource {
        root = ../.;
        fileset = cargoSourceFiles;
      };

      checkSrc = lib.fileset.toSource {
        root = ../.;
        fileset = lib.fileset.unions [
          cargoSourceFiles
          ../fixtures
        ];
      };

      commonBuildArgs = {
        inherit src;
        strictDeps = true;

        buildInputs = lib.optionals pkgs.stdenv.isDarwin [
          pkgs.libiconv
        ];
      };

      cargoArtifacts = craneLib.buildDepsOnly commonBuildArgs;

      individualCrateArgs = commonBuildArgs // {
        inherit cargoArtifacts;
        inherit (craneLib.crateNameFromCargoToml { inherit src; }) version;
        # NB: we disable tests since we'll run them all via cargo-nextest
        doCheck = false;
      };

      cli = craneLib.buildPackage (
        individualCrateArgs
        // rec {
          pname = "nix-search";
          cargoExtraArgs = "-p nix-search";
          meta.mainProgram = pname;
        }
      );
    in
    {
      checks = {
        clippy = craneLib.cargoClippy (
          commonBuildArgs
          // {
            src = checkSrc;
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          }
        );

        doc = craneLib.cargoDoc (
          commonBuildArgs
          // {
            inherit cargoArtifacts;
          }
        );

        rust-fmt = craneLib.cargoFmt {
          inherit src;
        };

        toml-fmt = craneLib.taploFmt {
          src = pkgs.lib.sources.sourceFilesBySuffices src [ ".toml" ];
        };

        rust-audit = craneLib.cargoAudit {
          inherit src;
          inherit (inputs) advisory-db;
        };

        rust-deny = craneLib.cargoDeny {
          inherit src;
        };

        rust-test = craneLib.cargoNextest (
          commonBuildArgs
          // {
            inherit cargoArtifacts;
            src = checkSrc;
            partitions = 1;
            partitionType = "count";
            cargoNextestPartitionsExtraArgs = "--no-tests=pass";
          }
        );
      };

      packages = rec {
        inherit cli;
        default = cli;
      };

      apps = rec {
        cli = {
          type = "app";
          program = lib.getExe self'.packages.cli;
        };
        default = cli;
      };

      devShells.default = craneLib.devShell {
        packages = with pkgs; [ watchexec ];
      };
    };
}
