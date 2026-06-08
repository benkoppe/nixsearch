{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      lib,
      self',
      config,
      ...
    }:
    let
      craneLib = inputs.crane.mkLib pkgs;
      webAssetSourceFiles = lib.fileset.fileFilter (
        file:
        file.hasExt "css" || file.hasExt "ico" || file.hasExt "js" || file.hasExt "svg" || file.hasExt "png"
      ) ../crates/web;
      cargoSourceFiles = lib.fileset.unions [
        (craneLib.fileset.commonCargoSources ../.)
        webAssetSourceFiles
      ];

      cargoDepsSourceFiles = craneLib.fileset.cargoTomlAndLock ../.;

      depsSrc = lib.fileset.toSource {
        root = ../.;
        fileset = cargoDepsSourceFiles;
      };

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

      cargoArtifacts = craneLib.buildDepsOnly (
        commonBuildArgs
        // {
          src = depsSrc;
          cargoExtraArgs = "--locked -p nixsearch";
        }
      );

      datastarJsPath = "${inputs.datastar}/bundles/datastar.js";

      datastarBuildEnv = {
        DATASTAR_JS_PATH = datastarJsPath;
      };

      workspaceBuildArgs = commonBuildArgs // datastarBuildEnv;

      individualCrateArgs = workspaceBuildArgs // {
        inherit cargoArtifacts;
        inherit (craneLib.crateNameFromCargoToml { inherit src; }) version;
        # NB: we disable tests since we'll run them all via cargo-nextest
        doCheck = false;
      };

      cli = craneLib.buildPackage (
        individualCrateArgs
        // rec {
          pname = "nixsearch";
          cargoExtraArgs = "--locked -p nixsearch";
          meta.mainProgram = pname;
        }
      );
    in
    {
      checks = {
        clippy = craneLib.cargoClippy (
          workspaceBuildArgs
          // {
            src = checkSrc;
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          }
        );

        doc = craneLib.cargoDoc (
          workspaceBuildArgs
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
          workspaceBuildArgs
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
        inherit cargoArtifacts;
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

      devShells.default = craneLib.devShell (
        {
          NIXSEARCH_CONFIG = "./nixsearch.example.toml";

          packages = [
            self'.packages.dev
          ];
        }
        // datastarBuildEnv
        // config.process-compose.dev.environmentVariables
      );
    };
}
