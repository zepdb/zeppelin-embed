# ZE-43 payload proposal review by ZE-38 owner

Read-only engineering review of the frozen proposal.md beside this report against
accepted storage/identity/writes contracts and reviewed ZE-38 WAL carriage.

Approve the physical shape and proposed tags11 PayloadChunk /12
OperationProvenance. The32-byte descriptor fields sum correctly;128*32+32=4128,
and128*65536=8MiB. Exact block refs include their24-byte frame header, while
chunk size counts only payload. Nonrecursive fixed-chunk addressing avoids
unbounded recursion and a materialized logical key. Exact role/version and
same-store/admitted-generation checks preserve the WAL required-reference seam.

No blocking defect found in the proposed format. Clarify these implementation
boundaries in evidence/tests before claiming them:

1. "No overlaps" must mean no malformed directory/logical extents or partial
   overlaps; repeated identical exact chunk refs are explicitly legal and must
   not be rejected. The two statements are consistent with this interpretation.
2. At-cap key tests include the9-byte kind/namespace prefix, so UTF-8 key bytes
   are at most8MiB-9. A GraphName constructor's independent limit is not a promise
   that a whole physical key/request fits. Keep the existing complete-bound rule.
3. Existing artifact::decode can scan a4MiB object without cancellation and
   creates owned FormatError strings on failure. A64KiB stream read loop cannot
   make an uncached resolver admission cancellable by polling only before it.
   Implement/retain explicit resolver accounting/control or state that boundary;
   cached already-validated frames can be borrowed without this new work.
4. Read-adapter malformed-input errors must use allocation-free io::ErrorKind
   conversion or charged fallible storage; io::Error::new with a message allocates.
   ZE-33 already caught this exact trap in an allocation test.
5. Extent stream carriage alone is not a decoder returning borrowed
   OperationFields across discontiguous chunks. Keep record/fence validation and
   any needed key reconstruction/borrowing an explicit owner responsibility.
   Do not claim this codec alone supplies lifecycle classification or metadata.

ZE-38 carries a required canonical reference whose root can be ExtentList8;
its mandatory resolver knows RequiredRole::Canonical and must validate every
chunk and the role/total before returning success. Missing middle extents must
reject the entire committed envelope. No competing mark/reclaim format or
publication owner is introduced. This review ran no tests/builds and is not
qualification evidence.
