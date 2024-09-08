{ pkgs ? import <nixpkgs> {} }:

with pkgs;

mkShell {
  packages = [
    # used for the cc toolchain
    clang_18
    # used for rust proto toolchain
    protobuf_23
    protoc-gen-tonic
    protoc-gen-prost
    protoc-gen-go
  ];
}
