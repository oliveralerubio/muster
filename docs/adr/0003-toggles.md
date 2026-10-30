# Toggle evaluation by layer

## Context

A machine or project may need fewer items than the team configuration. A later
layer must also be able to re-enable an item.

## Decision

Evaluate toggle lists in layer order. Within each layer, apply bare rules before
adapter-specific rules and disable before enable at each specificity. Keep the
lists separate instead of replacing an earlier list during merge.

A toggle controls deployment. `skills.lock` records review and provenance, so
turning a skill off does not change the lock or approve later source changes.
