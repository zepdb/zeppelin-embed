#  Windows build of the Node-API addon.
#
#  This file lives below the package root on purpose. A `binding.gyp` at the
#  package root would make npm's default install lifecycle invoke node-gyp
#  implicitly, which would fight the explicit Rust-first build orchestrated by
#  `scripts/build-native.mjs` and would break the macOS path entirely. The
#  orchestrator invokes this one with `--directory=native/windows`.
#
#  The Rust static implementation library and the generated C header are passed
#  in as gyp variables rather than guessed here, because their location depends
#  on CARGO_TARGET_DIR and on the selected target triple. The orchestrator
#  resolves both to absolute paths and passes them after `--`.
{
  'variables': {
    # Absolute path to zeppelin_embed_ffi.lib, the static *implementation*
    # archive. Deliberately not zeppelin_embed_ffi.dll.lib, which is the DLL's
    # import library and carries no implementation: linking that would produce
    # an addon that needs a Zeppelin DLL beside it at run time, which this
    # package does not ship.
    'ze_ffi_lib%': '',
    # Absolute path to crates/zeppelin-embed-ffi/include.
    'ze_ffi_include%': '',
  },
  'targets': [
    {
      'target_name': 'zeppelin_embed',
      'sources': ['../addon.cc'],
      'include_dirs': ['<(ze_ffi_include)'],
      'libraries': [
        '<(ze_ffi_lib)',
        # The system libraries rustc reports through
        # `--print native-static-libs` for this crate. Discovered, not
        # cargo-culted; see tasks/evidence/windows/w08-artifacts.md.
        '-lkernel32.lib',
        '-lntdll.lib',
        '-luserenv.lib',
        '-lws2_32.lib',
        '-ldbghelp.lib',
        '-ladvapi32.lib',
        '-lbcrypt.lib',
      ],
      'defines': [
        'NAPI_VERSION=8',
        # Keep the Windows headers lean and out of the way of the addon's own
        # identifiers.
        'WIN32_LEAN_AND_MEAN',
        'NOMINMAX',
      ],
      # node-gyp's delay-load hook. Electron's executable exports the Node
      # symbols itself rather than providing node.dll, so an addon built for
      # Electron must resolve them through the host process at load time. This
      # is what lets one addon binary load under both hosts.
      'win_delay_load_hook': 'true',
      'msvs_settings': {
        'VCCLCompilerTool': {
          # /EHsc
          'ExceptionHandling': 1,
          # RTTI disabled, matching the macOS build's -fno-rtti.
          'RuntimeTypeInfo': 'false',
          'AdditionalOptions': ['/std:c++17', '/W3'],
        },
        'VCLinkerTool': {
          # Node addons are DLLs with a .node extension.
          'ImageHasSafeExceptionHandlers': 'false',
        },
      },
      'configurations': {
        'Release': {
          'msvs_settings': {
            'VCCLCompilerTool': {
              # /MD: the dynamic CRT, matching what rustc links, so one process
              # never ends up holding two C runtimes.
              'RuntimeLibrary': 2,
              'Optimization': 2,
            },
          },
        },
        'Debug': {
          'msvs_settings': {
            'VCCLCompilerTool': {
              # /MDd
              'RuntimeLibrary': 3,
            },
          },
        },
      },
    },
  ],
}
