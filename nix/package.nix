{ lib, rustPlatform, makeWrapper, tippecanoe }:

rustPlatform.buildRustPackage {
  pname = "open-road-incidents";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../src ../tests ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  nativeBuildInputs = [ makeWrapper ];
  postInstall = ''
    wrapProgram $out/bin/open-road-incidents --prefix PATH : ${lib.makeBinPath [ tippecanoe ]}
  '';
  meta.mainProgram = "open-road-incidents";
}
