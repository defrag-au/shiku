{
  description = "shiku — declarative deploy platform for native Rust services on a Linux box";

  # nixpkgs + rust-overlay are pinned to the same revisions as the
  # augminted-bots workspace Shiku was extracted from, so the toolchain matches
  # and the Nix store cache is shared. Bump as needed; this repo is standalone
  # and does not depend on any private flake.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/1c3fe55ad329cbcb28471bb30f05c9827f724c76";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/3ecb5e6ab380ced3272ef7fcfe398bffbcc0f152";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        # Single source of truth: the toolchain (incl. the aarch64-musl target
        # the CLI cross-compiles to) comes from rust-toolchain.toml.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.cargo-zigbuild # cross-compile to aarch64-unknown-linux-musl
            pkgs.zig
            pkgs.just
            pkgs.rsync # release uploads (macOS openrsync is too old)
          ];
        };
      }
    );
}
