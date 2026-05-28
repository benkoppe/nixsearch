use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;

use nixsearch_core::ArtifactKind;
use nixsearch_store::{ArtifactMetadataInput, ArtifactStore};

use crate::artifact::{ProduceRequest, ProducedArtifact};

use super::Producer;
use super::nix::{
    create_tempdir, normalize_flake_ref, resolve_flake_revision, run_nix_build_expression,
};

#[derive(Debug, Clone)]
pub struct EvalModulesProducer {
    source_ref: String,
    inputs: BTreeMap<String, String>,
    modules: Vec<EvalModule>,
    producer_name: String,
}

#[derive(Debug, Clone)]
pub enum EvalModule {
    FlakeAttr(EvalModuleRef),
    ModuleListOption {
        option: String,
        modules: Vec<EvalModuleRef>,
    },
}

#[derive(Debug, Clone)]
pub struct EvalModuleRef {
    pub flake: String,
    pub attr: String,
}

impl EvalModulesProducer {
    pub fn new(
        source_ref: impl Into<String>,
        inputs: BTreeMap<String, String>,
        modules: Vec<EvalModule>,
    ) -> Self {
        Self {
            source_ref: source_ref.into(),
            inputs,
            modules,
            producer_name: "eval-modules".to_owned(),
        }
    }
}

#[async_trait]
impl Producer for EvalModulesProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let tempdir = create_tempdir("eval-modules")?;
        let expression_path = tempdir.path().join("eval-modules-options.nix");

        let source_ref = normalize_flake_ref(&self.source_ref)?;
        let inputs = normalize_eval_module_inputs(&self.inputs)?;
        let expression = eval_modules_expression(EvalModulesExpression {
            source_ref: &source_ref,
            inputs: &inputs,
            modules: &self.modules,
        });
        tokio::fs::write(&expression_path, expression)
            .await
            .with_context(|| {
                format!(
                    "failed to write eval-modules expression {}",
                    expression_path.display()
                )
            })?;

        let output_path = run_nix_build_expression(&expression_path)
            .await
            .with_context(|| {
                format!(
                    "failed to build options JSON for flake {:?}",
                    self.source_ref
                )
            })?;

        let options_json_path = output_path.join("share/doc/nixos/options.json");

        let bytes = tokio::fs::read(&options_json_path).await.with_context(|| {
            format!(
                "failed to read generated options JSON {}",
                options_json_path.display()
            )
        })?;

        let artifact_ref = request.artifact_ref(ArtifactKind::OptionsJson);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(source_ref.clone());

        match resolve_flake_revision(&source_ref).await {
            Ok(Some(revision)) => metadata_input.revision = Some(revision),
            Ok(None) => {}
            Err(error) => metadata_input.warnings.push(format!(
                "failed to resolve flake revision for source ref {source_ref:?}: {error:#}"
            )),
        }

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write eval-modules options artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

struct EvalModulesExpression<'a> {
    source_ref: &'a str,
    inputs: &'a BTreeMap<String, String>,
    modules: &'a [EvalModule],
}

fn eval_modules_expression(input: EvalModulesExpression<'_>) -> String {
    let explicit_inputs = eval_module_input_bindings(input.inputs);
    let modules = eval_module_list(input.modules);

    format!(
        r#"
let
  self = builtins.getFlake {source_ref};
  nixpkgs = if self ? inputs && self.inputs ? nixpkgs then self.inputs.nixpkgs else null;
  pkgs =
    if nixpkgs != null
    then nixpkgs.legacyPackages.${{builtins.currentSystem}}
    else import <nixpkgs> {{ }};
  lib = pkgs.lib;
  utils =
    if nixpkgs != null
    then import "${{nixpkgs}}/nixos/lib/utils.nix" {{
      inherit lib;
      config = {{ }};
      pkgs = null;
    }}
    else import <nixpkgs/nixos/lib/utils.nix> {{
      inherit lib;
      config = {{ }};
      pkgs = null;
    }};

  explicitInputs = {{
{explicit_inputs}
  }};

  getAttrPath = path: value:
    builtins.foldl'
      (acc: name: acc.${{name}})
      value
      path;

  resolveFlake = name:
    if name == "self" then self
    else if builtins.hasAttr name explicitInputs then builtins.getAttr name explicitInputs
    else getAttrPath (lib.splitString "." name) self;

  moduleAttr = flake: attr:
    getAttrPath (lib.splitString "." attr) (resolveFlake flake);

  moduleListOption = option: modules:
    {{ lib, ... }}:
    lib.setAttrByPath (lib.splitString "." option) modules;

  eval = lib.evalModules {{
    specialArgs = {{
      inherit pkgs lib utils;
      inputs = {{ self = self; }} // (self.inputs or {{}}) // explicitInputs;
      self = self;
    }};

    modules = [
{modules}
      ({{ lib, ... }}: {{
        options.users = lib.mkOption {{
          internal = true;
          type = lib.types.anything;
        }};

        config.users.users."<username>" = {{
          home = "/home/<username>";
        }};
        config.users.users."‹username›" = {{
          home = "/home/‹username›";
        }};
      }})
      ({{ lib, ... }}: {{
        options._module.args = lib.mkOption {{
          internal = true;
        }};
        config._module.check = false;
      }})
    ];
  }};
in
  (pkgs.nixosOptionsDoc {{
    inherit (eval) options;
    warningsAreErrors = false;
  }}).optionsJSON
"#,
        source_ref = nix_string(input.source_ref),
        explicit_inputs = explicit_inputs,
        modules = modules,
    )
}

fn eval_module_input_bindings(inputs: &BTreeMap<String, String>) -> String {
    inputs
        .iter()
        .map(|(name, source_ref)| {
            format!(
                "    {} = builtins.getFlake {};",
                nix_attr_name(name),
                nix_string(source_ref)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn eval_module_list(modules: &[EvalModule]) -> String {
    modules
        .iter()
        .map(eval_module_expression)
        .map(|module| indent_lines(&module, 6))
        .collect::<Vec<_>>()
        .join("\n")
}

fn eval_module_expression(module: &EvalModule) -> String {
    match module {
        EvalModule::FlakeAttr(module_ref) => eval_module_ref_expression(module_ref),
        EvalModule::ModuleListOption { option, modules } => {
            let modules = modules
                .iter()
                .map(eval_module_ref_expression)
                .map(|module| indent_lines(&module, 8))
                .collect::<Vec<_>>()
                .join("\n");

            format!(
                "(moduleListOption {} [\n{}\n])",
                nix_string(option),
                modules
            )
        }
    }
}

fn eval_module_ref_expression(module_ref: &EvalModuleRef) -> String {
    format!(
        "(moduleAttr {} {})",
        nix_string(&module_ref.flake),
        nix_string(&module_ref.attr)
    )
}

fn nix_attr_name(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn indent_lines(value: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);

    value
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn nix_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn normalize_eval_module_inputs(
    inputs: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    inputs
        .iter()
        .map(|(name, source_ref)| Ok((name.clone(), normalize_flake_ref(source_ref)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use nixsearch_core::{ArtifactKind, SearchDocument};
    use nixsearch_store::ArtifactStore;

    use crate::{
        Consumer, EvalModule, EvalModuleRef, EvalModulesProducer, OptionsJsonConsumer,
        ProduceRequest, Producer,
    };

    use super::{EvalModulesExpression, eval_modules_expression};

    #[tokio::test]
    #[ignore = "requires nix with flakes enabled"]
    async fn eval_modules_producer_builds_fixture_options_json() {
        let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = crate_dir
            .parent()
            .and_then(|path| path.parent())
            .expect("source crate should live under crates/source");

        let fixture_path = repo_root
            .join("fixtures")
            .join("eval-modules-flake")
            .canonicalize()
            .expect("eval-modules fixture should exist");

        let source_ref = format!("path:{}", fixture_path.display());

        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();

        let producer = EvalModulesProducer::new(
            source_ref.clone(),
            BTreeMap::new(),
            vec![EvalModule::FlakeAttr(EvalModuleRef {
                flake: "self".to_owned(),
                attr: "nixosModules.default".to_owned(),
            })],
        );

        let request = ProduceRequest {
            source: "eval-fixture".into(),
            ref_id: "local".into(),
        };

        let produced = producer.produce(&store, &request).await.unwrap();

        assert_eq!(produced.artifact_ref.kind, ArtifactKind::OptionsJson);
        assert_eq!(produced.metadata.producer, "eval-modules");
        assert_eq!(
            produced.metadata.source_url.as_deref(),
            Some(source_ref.as_str())
        );

        let consumer = OptionsJsonConsumer;
        let docs = consumer.consume(&store, &produced).await.unwrap();

        let fixture_option = docs
            .iter()
            .find(|doc| doc.name() == "programs.fixture.enable")
            .expect("expected fixture option in docs");

        let SearchDocument::Option(option) = fixture_option else {
            panic!("expected fixture option to be an option document");
        };

        assert!(
            !option.declarations.is_empty(),
            "expected fixture option to have source declarations: {option:#?}"
        );
    }

    #[test]
    fn eval_modules_expression_contains_flake_ref_module_attr_and_default_eval_args() {
        let expression = eval_modules_expression(EvalModulesExpression {
            source_ref: "github:example/project",
            inputs: &BTreeMap::new(),
            modules: &[EvalModule::FlakeAttr(EvalModuleRef {
                flake: "self".to_owned(),
                attr: "nixosModules.default".to_owned(),
            })],
        });

        assert!(expression.contains("\"github:example/project\""));
        assert!(expression.contains("(moduleAttr \"self\" \"nixosModules.default\")"));
        assert!(expression.contains("builtins.getFlake"));
        assert!(expression.contains("lib.evalModules"));
        assert!(expression.contains("specialArgs"));
        assert!(expression.contains("inherit pkgs lib utils;"));
        assert!(expression.contains("self.inputs.nixpkgs"));
        assert!(expression.contains("legacyPackages.${builtins.currentSystem}"));
        assert!(
            expression
                .contains("inputs = { self = self; } // (self.inputs or {}) // explicitInputs;")
        );
        assert!(expression.contains("config._module.check = false;"));
        assert!(expression.contains("pkgs.nixosOptionsDoc"));
    }

    #[test]
    fn eval_modules_expression_supports_explicit_inputs_and_module_list_options() {
        let inputs = BTreeMap::from([(
            "dependency".to_owned(),
            "github:example/dependency".to_owned(),
        )]);

        let expression = eval_modules_expression(EvalModulesExpression {
            source_ref: "github:example/project",
            inputs: &inputs,
            modules: &[
                EvalModule::FlakeAttr(EvalModuleRef {
                    flake: "dependency".to_owned(),
                    attr: "nixosModules.default".to_owned(),
                }),
                EvalModule::ModuleListOption {
                    option: "example.extraModules".to_owned(),
                    modules: vec![EvalModuleRef {
                        flake: "self".to_owned(),
                        attr: "modules.extra".to_owned(),
                    }],
                },
            ],
        });

        assert!(
            expression
                .contains("\"dependency\" = builtins.getFlake \"github:example/dependency\";")
        );
        assert!(expression.contains("(moduleAttr \"dependency\" \"nixosModules.default\")"));
        assert!(expression.contains("(moduleListOption \"example.extraModules\" ["));
        assert!(expression.contains("(moduleAttr \"self\" \"modules.extra\")"));
    }

    #[test]
    fn eval_modules_expression_resolves_primary_flake_inputs() {
        let expression = eval_modules_expression(EvalModulesExpression {
            source_ref: "github:example/project",
            inputs: &BTreeMap::new(),
            modules: &[EvalModule::FlakeAttr(EvalModuleRef {
                flake: "self.inputs.hjem".to_owned(),
                attr: "nixosModules.default".to_owned(),
            })],
        });

        assert!(expression.contains("(moduleAttr \"self.inputs.hjem\" \"nixosModules.default\")"));
    }

    #[test]
    fn nix_string_escapes_values() {
        let escaped = super::nix_string("hello\"world");

        assert_eq!(escaped, "\"hello\\\"world\"");
    }
}
