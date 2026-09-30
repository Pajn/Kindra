# The journal records its steps and a cursor

Status: accepted

Implemented in journal version 4 (`Step` and `Cursor` in `src/rebase_utils.rs`).

An operation's journal stores the list of steps it will perform and the position reached, and `kin continue` resumes from that position. Today, progress is inferred from the commit graph: a branch counts as done when it already descends from its new base. That inference cannot describe steps that are not "replay this branch onto that base", such as autosquashing a range or moving one commit onto an ancestor. Those steps therefore run outside the restack loop, each with its own save, conflict handoff and rollback. Graph checks remain, but only to verify that a step the user finished with Git directly, for example with `git rebase --continue`, really completed.

## Consequences

- The journal is versioned from its first release, so later format changes can be detected rather than guessed.
- A paused operation recorded by Kindra 1.1 or earlier stays usable after an upgrade. Operations built only from branch replays (move, restack, reorder and sync) are read by a legacy adapter and can be continued. Every other operation recorded by 0.2.0 or later can at least be aborted. Kindra 0.1.0 did not record which branches an operation owns, so `kin abort` cannot prove what it would undo; for those it refuses and points to `kin continue` or `kin abort --clear-state`.
- Checkout hydration records its plan the same way, as steps and a cursor, but in its own journal (`kindra_checkout_state.json`, versioned separately). Its steps create branches at recorded commits and check one out; they are not rebases, so they do not go through the rebase loop.
- Inferring progress from the graph was rejected because it can only describe branch replays. It would keep the autosquash and move-onto-ancestor rebases, and their separate recovery paths, outside the shared executor.
