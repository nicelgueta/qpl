# Releases

Releases are automated and driven entirely by the version number. A version
bump in `Cargo.toml` landing on `main` causes GitHub Actions to tag the
commit, cross-compile, and publish binaries to the
[releases page](https://github.com/nicelgueta/qpl/releases).

Five targets are built each time: Linux x86 against gnu and against musl,
Linux ARM64, and macOS on both Intel and Apple silicon. The musl build is the
one to take if you want something that will run in essentially any container
without worrying about the host's C library.
