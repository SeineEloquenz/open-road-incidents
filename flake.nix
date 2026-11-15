{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs, ... }:
    let
      forAll = f: nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ] (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        default = open-road-incidents;
        open-road-incidents = pkgs.callPackage ./nix/package.nix { };
        valhalla = pkgs.callPackage ./nix/valhalla.nix { };
        build-graph = pkgs.writeShellApplication {
          name = "build-graph";
          runtimeInputs = [ valhalla pkgs.curl pkgs.coreutils ];
          text = builtins.readFile ./scripts/build-graph;
        };
      });

      nixosModules.default = { pkgs, lib, ... }:
        let
          packages = self.packages.${pkgs.stdenv.hostPlatform.system};
        in
        {
          imports = [ ./nix/module.nix ];
          services.open-road-incidents.package = lib.mkDefault packages.open-road-incidents;
          services.open-road-incidents.valhalla.package = lib.mkDefault packages.valhalla;
        };

      checks = forAll (pkgs: {
        module = (nixpkgs.lib.nixosSystem {
          inherit (pkgs.stdenv.hostPlatform) system;
          modules = [
            self.nixosModules.default
            {
              boot.loader.grub.enable = false;
              fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
              system.stateVersion = "25.11";
              services.open-road-incidents = {
                enable = true;
                virtualHost = "roads.example.org";
              };
            }
          ];
        }).config.system.build.toplevel;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            tippecanoe
            self.packages.${pkgs.stdenv.hostPlatform.system}.valhalla
          ];
        };
      });
    };
}
