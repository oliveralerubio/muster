# Session usage privacy

The local usage report reads transcripts locally and does not contact a remote
service or write any local state or transcript cache. It reports validated item
names (`[A-Za-z0-9._-]`, up to 64 characters), aggregate counts, timestamps,
source directories, and skip counts (`older_than_window`, `too_large`,
`unreadable`). Session identifiers are SHA-256 hashed in memory for counting
and never emitted. Transcript file and directory paths are never emitted
either, not even in skip reports: transcript names contain session ids and
encoded project paths, so skips are reported as per-reason counts only. Prompt
text, assistant responses, tool inputs/arguments, and tool results are never
included in stdout, stderr, or stored on disk. Source coverage is incomplete,
so an unused suggestion is not proof that an item is safe to disable.
