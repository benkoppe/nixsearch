{
  description = "nixsearch eval-modules fixture";

  outputs =
    { self }:
    {
      nixosModules.default = ./module.nix;
    };
}
