# Zeppelin Embed macOS SDK

This archive contains the Zeppelin Embed core C ABI for macOS on Apple
silicon. It can be called from any language that supports C-compatible foreign
functions.

- `include/zeppelin_embed.h`: public C header
- `lib/libzeppelin_embed_ffi.a`: static library
- `lib/libzeppelin_embed_ffi.dylib`: dynamic library

The v0.1 SDK provides caller-supplied vector, lexical, and hybrid retrieval. It
does not contain embedding models or the optional model-backed text API.

The dylib uses the install name `@rpath/libzeppelin_embed_ffi.dylib`. Embed it
in the application, add the containing directory to the executable's runtime
search paths, and sign the finished application bundle. A C program can link
the static archive with `-liconv`; Clang supplies the remaining macOS system
libraries.

See <https://github.com/zepdb/zeppelin-embed> for API examples and source.

Build with `--artifact legacy` (the default) or `--artifact graph-cypher`.
The graph-cypher SDK supports macOS 14+ arm64, includes both core and graph
headers, and installs `libzeppelin_embed_graph_cypher_ffi.a` and `.dylib`.
Its install name is `@rpath/libzeppelin_embed_graph_cypher_ffi.dylib`.
The legacy SDK retains macOS 11+ arm64 and its existing filenames.

These SDKs are alternatives. Graph already includes the full core and compiler;
link one artifact, never both archives. Shipping excludes optional text and
runner test hooks. Early footprint evidence does not qualify the final release
or the actual minimum-runtime consumer (ZE-71/78).
