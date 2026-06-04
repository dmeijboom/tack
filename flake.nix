{
  description = "tack - a Kubernetes context and namespace switcher";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage {
          pname = "tack";
          version = (nixpkgs.lib.importTOML ./Cargo.toml).package.version;

          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "A Kubernetes context and namespace switcher";
            homepage = "https://github.com/dmeijboom/tack";
            license = nixpkgs.lib.licenses.mit;
            mainProgram = "tack";
          };
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.system}.default ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
