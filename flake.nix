{
  description = "TermLRC - Synced-lyrics terminal showcase with Kitty pixel-level scrolling, MPRIS, recursive LRC search, and remote lyric fallbacks";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          termlrc = pkgs.rustPlatform.buildRustPackage {
            pname = "termlrc";
            version = "0.1.0";

            src = pkgs.lib.cleanSource ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = [
              pkgs.makeWrapper
            ];

            postInstall = ''
              wrapProgram $out/bin/termlrc \
                --suffix PATH : ${pkgs.lib.makeBinPath [ pkgs.fontconfig ]}
            '';

            meta = with pkgs.lib; {
              description = "Synced-lyrics terminal showcase with Kitty pixel-level scrolling, MPRIS, recursive LRC search, and remote synced lyric fallbacks";
              homepage = "https://github.com/MaveDX/TermLRC";
              license = licenses.mit;
              mainProgram = "termlrc";
              platforms = platforms.linux;
            };
          };
        in
        {
          default = termlrc;
          inherit termlrc;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/termlrc";
        };
      });

      overlays.default = final: _prev: {
        termlrc = self.packages.${final.system}.default;
      };

      devShells = forAllSystems (system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
              fontconfig
            ];
          };
        }
      );
    };
}

