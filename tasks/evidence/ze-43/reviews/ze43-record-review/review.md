# ZE43 record proposal review

Read-only review of frozen proposal.md, no source edits or builds. Header
geometry (node40, relationship80), property-row24 and PayloadRef48 arithmetic
is consistent. Canonical value spans and typed text/vector views can preserve
lossless identity without duplicate payloads. Explicit discontiguous provenance
readers correctly avoid inventing an uncharged contiguous borrowed string.

Two verification obligations should be made explicit before implementation:

1. Property binding must be a bijection: record row count equals the canonical
   property count, every canonical property occurs exactly once, and each row's
   symbol resolves to the exact name at that value boundary. Checking only that
   each supplied index lands on a valid canonical value permits omission of a
   canonical property, which would silently drop typed lookup results. Include a
   checksum-repaired missing-row case and a same-count wrong-name mapping case.
2. Duplicate scalar metadata must bind to canonical data too: the numeric node
   label set resolves to exactly the canonical label-name set, and relationship
   source/target IDs and relationship-type symbol/name equal the canonical
   relationship contents. Property-only binding is insufficient. Include missing
   label, wrong endpoint and wrong type mapping controls with valid framing.

These do not change the proposed byte geometry. Approved as a physical layout
proposal subject to these whole-record verification obligations and the stated
budget/cancellation/mandatory leaf-validation constraints. No runtime acceptance
or real storage admission is implied by this document review.

## Derived record size follow-up

Root proposed PayloadRef48 (not naked PhysicalRef32) as directory values for
node/relationship records and a role-specific32MiB logical derived-record cap.
Approved: records may expand from symbol/value indexes while canonical input
limits remain unchanged. ZGEX supports up to512 record chunks (32+512*32=16416
bytes); other original payload roles retain128 chunks/8MiB. The same existing
actual32MiB shared preparation budget still includes retained backing, sink
capacity and live scratch; this does not create an additional allowance. Typed
role validation must prevent a forged record role from relaxing canonical or
provenance limits. No recursion or whole-record copy is introduced.
