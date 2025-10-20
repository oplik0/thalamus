{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  # https://devenv.sh/packages/
  packages = [
    pkgs.git
    pkgs.jq
    pkgs.curl
  ];

  # https://devenv.sh/languages/
  languages.rust.enable = true;
  languages.typescript.enable = true;

  # https://devenv.sh/processes/
  # processes.dev.exec = "${lib.getExe pkgs.watchexec} -n -- ls -la";

  # https://devenv.sh/services/
  services.postgres.enable = true;

  # https://devenv.sh/basics/
  enterShell = '''';

  # https://devenv.sh/tasks/
  # tasks = {
  #   "myproj:setup".exec = "mytool build";
  #   "devenv:enterShell".after = [ "myproj:setup" ];
  # };

  # https://devenv.sh/tests/
  enterTest = ''
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # https://devenv.sh/git-hooks/
  git-hooks = {
    package = pkgs.prek;
    hooks = {
      editorconfig-checker.enable = true;
      nixfmt.enable = true;
      clippy.enable = true;
      rustfmt.enable = true;
      # todo: uncomment once frontend is set up
      # biome.enable = true;
      end-of-file-fixer.enable = true;
      trim-trailing-whitespace.enable = true;
      check-added-large-files.enable = true;
      check-json.enable = true;
      mixed-line-endings.enable = true;
    };
  };

  # other integrations
  delta.enable = true;
  devcontainer.enable = true;
  # See full reference at https://devenv.sh/reference/options/
}
