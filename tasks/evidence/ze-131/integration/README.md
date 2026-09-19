# ZE-131 main integration

Source ed7ce4d1dfccc29a2071c66ab449013a2893f7a0 applied onto main95a92472d39505ac8bf0357b7316226526408c91. All31 candidate paths are byte-identical; the ZE-132 fixture changes are disjoint. All45 inherited files match preservation.json. No push.

Main focused verification on this macOS arm64 host: isolated boundary/routing script PASS (including actual installed x86_64 legacy success and intended graph unsupported-target failure); legacy ANN10, default registry1, ordinary graph registry2, explicit hook registry plus actual paired PG16 probe3, graph FFI contracts/header/layout21:37nextest tests pass at-j4. Exact commands, timings and exit status are in checks.json. Raw logs are preserved byte-for-byte in raw-checks.tar.gz; readable logs trim only trailing blank lines.

Independent Sol/xhigh boundary review passed on exact source: real runtime receipt counts88default/235graph/247hook,17probes,162string inventory and21/3/6gated targets preserved. Root reviewed tooling and found old candidate380ea1a routed graph on unsupported legacy hosts/target overrides: independent12-route RED had7failures. Sourceed7 fixes the routing; the expanded independent15-route matrix is entirely GREEN, including arm64 host with x86selectedrustc. Stub routes validate command selection only, not foreign execution.

The exact candidate owner's scoped checks/lint and RED/GREEN are recorded in ../README.md. No production bytes changed during main integration. Nonessential broad workspace/adversarial/coverage/size campaigns remain ZE-118. Native graph packaging/public runtime/Swift/complete reachable footprint and final5120KiB qualification remain ZE-107/69/71/78. Neither dependency selection nor workspace feature unification proves a shipping standalone artifact.
