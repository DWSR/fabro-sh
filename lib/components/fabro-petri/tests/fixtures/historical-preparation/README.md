# Historical preparation admission

`graph.json` is the verbatim serialized graph admitted by Petri revision
`50809f9bd2847d20f047c3d5ffc6692fbc2f803d`, before removal of `run.prepare`.
`spec.json` wraps that graph reference and display graph in Fabro's saved RunSpec
shape, including the retired `settings.run.prepare` projection. It uses synthetic
identity and secret references only.

The original workflow was `start -> work -> exit`; preparation inserted
`run_prepare_1`. Its command writes `prepared` after checking a literal MODE
overlay and a vault TOKEN overlay, overriding both workflow environment keys.
The five-minute command timeout, failure policy, graph bytes, digest, and blob
hash are frozen. Stage-complete hooks record their node names in `stage-hooks`.

Do not regenerate these fixtures with a newer frontend: the tests load the
saved admission directly to exercise history, resume, retry and hook policy.
