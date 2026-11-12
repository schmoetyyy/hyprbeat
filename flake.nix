{
  description = "hyprbeat — a desktop-background rhythm game for Hyprland/Wayland";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux"; # add aarch64-linux when hardware demands it
      pkgs = nixpkgs.legacyPackages.${system};

      # Runtime libraries:
      #  - wayland: wl client protocols (sctk builds against the crate, but
      #    libwayland is dlopened by some tooling and useful for WAYLAND_DEBUG)
      #  - libxkbcommon: sctk keyboard handling (probe mode; phase 4 input)
      #  - vulkan-loader: wgpu dlopens this at runtime
      #  - alsa-lib: cpal speaks ALSA directly on Linux (phase 2)
      runtimeLibs = with pkgs; [
        wayland
        libxkbcommon
        vulkan-loader
        alsa-lib
      ];
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          rustc
          cargo
          clippy
          rustfmt
          rust-analyzer
          pkg-config
        ] ++ runtimeLibs;

        # wgpu's Vulkan backend loads libvulkan at runtime; inside the
        # devshell, point the loader at our libraries. libasound covers
        # cpal's ALSA host (PipeWire's ALSA node satisfies it at runtime).
        shellHook = ''
          export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath runtimeLibs}:$LD_LIBRARY_PATH"
          export RUST_LOG="hyprbeat=info"
          echo "hyprbeat devshell — try: cargo run -- --audio assets/test-tone.wav --timeout 12"
          echo "phase-1 validation: dev/validate-phase1.sh"
          echo "phase-2 validation: dev/validate-phase2.sh"
        '';
      };

      # packages.${system}.hyprbeat — the proper flake package output (naersk
      # / crane + runtimeDependencies wiring) is phase 8 work (§12.1).
    };
}
