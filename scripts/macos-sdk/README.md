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
