---
type: wiki-page
id: policy-iteration
created: 2026-05-21T10:30:00Z
updated: 2026-05-22T11:25:00Z
summary: "Classic dynamic-programming loop alternating policy evaluation and policy improvement; AlphaZero realizes it with MCTS as the improvement operator"
topic: policy-iteration
tags: [reinforcement-learning, dynamic-programming, policy-iteration]
derived_from:
  - [[records/papers/silver-alphazero-2017]]
---

# Policy iteration

Policy iteration is the classic dynamic-programming scheme that
alternates two steps until the policy stops changing: *policy
evaluation* (compute the value function of the current policy) and
*policy improvement* (make the policy greedy with respect to that
value function).

## Generalized form

Generalized policy iteration interleaves the two steps at any
granularity rather than running each to convergence. Most modern RL
algorithms are an instance of it.

## In AlphaZero

AlphaZero ([[records/papers/silver-alphazero-2017]]) frames its
training as policy iteration where [[wiki/concepts/mcts]] is the
improvement operator: search produces a stronger policy than the raw
network, and the network is trained toward the search result. The loop
is driven entirely by [[wiki/concepts/self-play]].

## Related concepts

- [[wiki/concepts/value-network]]
- [[wiki/concepts/markov-decision-process]]
