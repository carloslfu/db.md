---
type: wiki-page
id: self-play
created: 2026-05-20T15:00:00Z
updated: 2026-05-22T11:15:00Z
summary: "Training regime where an RL agent generates its own data by playing copies of itself — no expert games, no hand-coded heuristics; powers TD-Gammon and AlphaZero"
topic: self-play
tags: [reinforcement-learning, self-play, training]
derived_from:
  - [[records/papers/silver-alphazero-2017]]
  - [[records/papers/tesauro-tdgammon-1995]]
---

# Self-play

Self-play is a training regime where a reinforcement-learning agent
generates its own training data by playing against copies of itself,
without external expert games or hand-coded heuristics.

## Mechanism

The agent plays against a (possibly older, possibly current) version
of itself. Game outcomes label the trajectories. Policy and value
networks are updated against these labels. Over training, the agent
moves through a curriculum of increasingly competent opponents — itself.

## History

Tesauro's TD-Gammon ([[records/papers/tesauro-tdgammon-1995]]) was the
first famous success: a neural-network backgammon player trained by
self-play that reached world-class strength.

AlphaZero ([[records/papers/silver-alphazero-2017]]) generalized the
recipe: MCTS + neural networks + pure self-play, no human data,
master-level chess, shogi, and Go.

## Open questions

- *Stability vs diversity:* pure self-play can converge on a single
  exploit and stop improving. Population-based methods (League of
  StarCraft II agents) trade compute for diversity.
- *Transfer:* whether self-play in one game transfers to related
  games is unresolved.

## Related concepts

- [[wiki/concepts/mcts]]
- [[wiki/concepts/policy-iteration]]
- [[wiki/concepts/value-network]]
