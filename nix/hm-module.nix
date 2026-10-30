# Home Manager module: `programs.eloquawk`. Takes this flake (`self`) for its package.
#
# Installing the package is all it does: the package carries the desktop entry (which
# registers Eloquawk for `mailto:` links) and the hicolor icons. Eloquawk has no tray and no
# background service, so there is no systemd unit to manage.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.eloquawk;
  inherit (pkgs.stdenv.hostPlatform) system;
in
{
  options.programs.eloquawk = {
    enable = lib.mkEnableOption "Eloquawk, a native Gmail client";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.default;
      defaultText = lib.literalExpression "eloquawk.packages.\${system}.default";
      description = "The Eloquawk package.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
  };
}
