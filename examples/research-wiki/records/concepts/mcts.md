---
type: concept
meta-type: conclusion
id: mcts
created: 2026-05-21T10:00:00Z
updated: 2026-05-23T16:45:00Z
summary: "Monte Carlo Tree Search: a best-first search that builds an asymmetric tree via repeated simulation; in AlphaZero a neural net replaces the random rollout"
topic: monte-carlo-tree-search
tags: [reinforcement-learning, search, planning, mcts]
derived_from:
  - [[records/papers/silver-alphazero-2017]]
  - [[records/papers/silver-alphago-2016]]
---

# Monte Carlo Tree Search

Monte Carlo Tree Search (MCTS) is a best-first search algorithm that
incrementally builds an asymmetric search tree, spending more
simulation on the most promising lines of play.

## Mechanism

Each iteration runs four phases: *selection* (descend the tree by a
tree policy such as UCT), *expansion* (add a child node), *simulation*
(estimate the value of the new node), and *backpropagation* (update
statistics up the path). Classic MCTS estimates value with a random
rollout to a terminal state. The UCT selection rule is itself an
[[records/concepts/exploration-exploitation]] mechanism: it adds an
optimism bonus to under-visited children so the search does not commit
to one line too early.

## In AlphaGo and AlphaZero

AlphaGo ([[records/papers/silver-alphago-2016]]) first paired MCTS with
learned networks. AlphaZero ([[records/papers/silver-alphazero-2017]])
replaces the random rollout with a neural network that outputs a value
estimate and a policy prior, so the search is guided rather than random.
The network is a deep [[records/concepts/function-approximation]] of the
position. The search result, in turn, is the training target for the
network — the policy-iteration loop.

## Related concepts

- [[records/concepts/policy-iteration]]
- [[records/concepts/value-network]]
- [[records/concepts/self-play]]
- [[records/concepts/exploration-exploitation]]
- [[records/concepts/function-approximation]]
