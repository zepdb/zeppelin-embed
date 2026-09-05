# Step 17 singleton admission screen

The zero/singleton exact path removes mostly-unique-term regressions on all
three tombstoned geometries. All-live controls still show 0.041-0.500 us
additional p95. A final revision restores the original frequency loop for
snapshots with no frequency cache, bypassing fallible cache accumulation.
This intermediate implementation is not the final retained disposition.

Same harness, fixed before binary and AB/BA/AB protocol as `../astra-17-measured`.
All 18,432 timed calls and 96,192 returned hits preserve exact controls.

| Rows | Segments | Live | Workload | Before p95 us | After p95 us | Change |
| ---: | ---: | ---: | --- | ---: | ---: | ---: |
| 8192 | 1 | 8192 | warm-common | 1.334 | 1.458 | +9.30% |
| 8192 | 1 | 8192 | repeated-groups | 9.459 | 9.250 | -2.21% |
| 8192 | 1 | 8192 | mostly-unique | 1.125 | 1.250 | +11.11% |
| 8192 | 1 | 8192 | absent | 0.625 | 0.708 | +13.28% |
| 8192 | 1 | 6144 | warm-common | 34.125 | 1.375 | -95.97% |
| 8192 | 1 | 6144 | repeated-groups | 11.584 | 9.833 | -15.12% |
| 8192 | 1 | 6144 | mostly-unique | 1.250 | 1.209 | -3.28% |
| 8192 | 1 | 6144 | absent | 0.625 | 0.625 | +0.00% |
| 8192 | 8 | 8192 | warm-common | 4.584 | 4.584 | +0.00% |
| 8192 | 8 | 8192 | repeated-groups | 20.541 | 20.250 | -1.42% |
| 8192 | 8 | 8192 | mostly-unique | 3.125 | 3.167 | +1.34% |
| 8192 | 8 | 8192 | absent | 1.500 | 1.625 | +8.33% |
| 8192 | 8 | 6144 | warm-common | 71.458 | 4.917 | -93.12% |
| 8192 | 8 | 6144 | repeated-groups | 27.083 | 21.667 | -20.00% |
| 8192 | 8 | 6144 | mostly-unique | 3.333 | 3.292 | -1.23% |
| 8192 | 8 | 6144 | absent | 1.625 | 1.667 | +2.58% |
| 65536 | 1 | 65536 | warm-common | 1.125 | 1.250 | +11.11% |
| 65536 | 1 | 65536 | repeated-groups | 65.625 | 62.375 | -4.95% |
| 65536 | 1 | 65536 | mostly-unique | 1.750 | 2.250 | +28.57% |
| 65536 | 1 | 65536 | absent | 0.417 | 0.458 | +9.83% |
| 65536 | 1 | 49152 | warm-common | 261.583 | 1.209 | -99.54% |
| 65536 | 1 | 49152 | repeated-groups | 80.291 | 65.125 | -18.89% |
| 65536 | 1 | 49152 | mostly-unique | 2.292 | 1.875 | -18.19% |
| 65536 | 1 | 49152 | absent | 0.416 | 0.417 | +0.24% |
