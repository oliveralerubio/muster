# Session usage privacy

The local usage report reads transcripts locally and does not contact a remote
service or write any local state or transcript cache. It reports validated item
names (`[A-Za-z0-9._-]`, up to 64 characters) and aggregate counts, timestamps,
source paths, and skip reasons (`older_than_window`, `too_large`, `unreadable`).
Session identifiers are SHA-256 hashed in memory for counting and never emitted.
Prompt text, assistant responses, tool inputs/arguments, and tool results are
never included in stdout, stderr, or stored on disk. Source paths may themselves
contain identifying information; treat a report as local data. Source coverage
is incomplete, so an unused suggestion is not proof that an item is safe to
disable.

