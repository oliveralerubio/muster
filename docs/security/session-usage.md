# Session usage privacy

The local usage report reads transcripts locally and does not contact a remote
service or write a transcript cache. It reports item names and aggregate counts,
timestamps, source paths, and skip reasons. Session identifiers are hashed for
counting. Prompt and response content and tool arguments are not included in the
report. Source paths may themselves contain identifying information; treat a
report as local data. Source coverage is incomplete, so an unused suggestion is
not proof that an item is safe to disable.
