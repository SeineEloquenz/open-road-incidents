{ lib, symlinkJoin, makeWrapper, valhalla, libsodium, fetchFromGitHub }:

let
  # nixpkgs' 3.9.0 aborts while building tiles for Germany.
  pinned = valhalla.overrideAttrs (finalAttrs: old: {
    version = "3.9.1";
    src = fetchFromGitHub {
      owner = "valhalla";
      repo = "valhalla";
      tag = finalAttrs.version;
      hash = "sha256-qgp4lkZ414fe5/X88e8po3ynygcoUN3UM+R1hbz00WY=";
      fetchSubmodules = true;
    };
  });
in
# nixpkgs' Valhalla links libsodium without a runpath to it and fails to start.
symlinkJoin {
  name = "valhalla-${pinned.version}";
  paths = [ pinned ];
  nativeBuildInputs = [ makeWrapper ];
  postBuild = ''
    for bin in $out/bin/*; do
      wrapProgram "$bin" --prefix LD_LIBRARY_PATH : ${libsodium}/lib
    done
  '';
}
